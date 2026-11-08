// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/debug/zxdb/client/fdf_dispatcher_task_provider.h"

#include <algorithm>
#include <cstring>
#include <optional>
#include <set>
#include <string>
#include <string_view>
#include <utility>
#include <vector>

#include "src/developer/debug/shared/message_loop.h"
#include "src/developer/debug/shared/string_util.h"
#include "src/developer/debug/zxdb/client/frame.h"
#include "src/developer/debug/zxdb/client/session.h"
#include "src/developer/debug/zxdb/client/thread.h"
#include "src/developer/debug/zxdb/common/err.h"
#include "src/developer/debug/zxdb/common/join_callbacks.h"
#include "src/developer/debug/zxdb/common/ref_ptr_to.h"
#include "src/developer/debug/zxdb/common/string_util.h"
#include "src/developer/debug/zxdb/expr/eval_context.h"
#include "src/developer/debug/zxdb/expr/expr.h"
#include "src/developer/debug/zxdb/expr/expr_parser.h"
#include "src/developer/debug/zxdb/expr/expr_value.h"
#include "src/developer/debug/zxdb/expr/resolve_collection.h"
#include "src/developer/debug/zxdb/expr/resolve_ptr_ref.h"
#include "src/developer/debug/zxdb/symbols/collection.h"
#include "src/developer/debug/zxdb/symbols/data_member.h"
#include "src/developer/debug/zxdb/symbols/function.h"
#include "src/developer/debug/zxdb/symbols/identifier.h"
#include "src/developer/debug/zxdb/symbols/symbol_utils.h"
#include "src/developer/debug/zxdb/symbols/template_parameter.h"
#include "src/developer/debug/zxdb/symbols/type.h"
#include "src/developer/debug/zxdb/symbols/value.h"
#include "src/lib/fxl/memory/ref_ptr.h"
#include "src/lib/fxl/memory/weak_ptr.h"
#include "src/lib/fxl/strings/string_number_conversions.h"

namespace zxdb {

namespace {

// Overview of the Fuchsia Driver Runtime Task Architecture
// ========================================================
//
// In the Fuchsia Driver Framework, fdf::Dispatcher is backed by
// driver_runtime::Dispatcher.
//
// A driver_runtime::Dispatcher stores its pending work in four intrusive
// doubly-linked lists of driver_runtime::CallbackRequest objects:
//   - callback_queue_: Tasks and callbacks that have been triggered and are
//     ready to run, reported with state Pending.
//   - delayed_tasks_: Instances of driver_runtime::DelayedTask waiting for
//     their deadline to expire, reported with state Delayed.
//   - always_on_delayed_tasks_: Delayed tasks on always-on dispatchers, also
//     reported with state Delayed.
//   - registered_callbacks_: Callbacks registered with the dispatcher, such as
//     pending fdf::ChannelRead or async::Wait operations, that have not yet
//     been triggered, reported with state Waiting.
//
// Each queue is an fbl::DoublyLinkedList<std::unique_ptr<CallbackRequest>>.
// The list stores its first element in DoublyLinkedListBase::head_, and each
// CallbackRequest inherits from fbl::DoublyLinkedListable, which embeds
// dll_node_state_.next_ and dll_node_state_.prev_. In fbl intrusive containers,
// an empty list head_ and the tail node next_ pointer are marked by a sentinel
// pointer value with bit 0 set.
//
// Inside each driver_runtime::CallbackRequest:
//   - When a task is posted via async::PostTask, async::PostDelayedTask,
//     async::TaskClosure, or async::Task, driver_runtime::Dispatcher::PostTask
//     creates a CallbackRequest and passes the async_task_t* pointer to
//     CallbackRequest::SetCallback. SetCallback stores that pointer in the
//     std::optional<void*> member async_operation_, and also captures the task
//     pointer in the lambda stored in CallbackRequest::callback_.
//   - The async_task_t struct contains state, handler, and deadline. When
//     posted through C++ libasync wrappers, handler points to a static
//     trampoline such as async::internal::RetainedTask::Handler,
//     async::TaskClosure::CallHandler, or async::Task::CallHandler. Because
//     RetainedTask and async::TaskBase embed async_task_t as their first
//     member, resolving the async_task_t* address as the enclosing wrapper
//     struct exposes its handler or handler_ member.
//   - For direct driver_runtime callbacks that do not wrap an async_task_t,
//     such as fdf::ChannelRead, CallbackRequest::callback_ directly stores the
//     user driver_runtime::Callback.
//
// Every fit::function, fit::closure, and fit::inline_callback inherits from
// fit::internal::generic_function_base, which stores:
//   - bits_: An inline byte buffer holding either the Callable object itself
//     or a heap pointer to Callable.
//   - ops_: A pointer to fit::internal::target_ops, whose invoke member points
//     to fit::internal::target<Callable, ...>::invoke.

// Bit 0 is set by fbl::internal::make_sentinel to mark the end sentinel of an
// fbl::DoublyLinkedList.
constexpr uint64_t kContainerSentinelBit = 1ULL;

// Maximum number of nodes to visit in a single fbl::DoublyLinkedList queue to
// guard against cycles if target memory is corrupted.
constexpr size_t kMaxQueueNodes = 256;

class FdfDispatcherTask : public AsyncTask {
 public:
  FdfDispatcherTask(Session* session, uint64_t id, Location location, Identifier identifier,
                    std::string state, AsyncTask::Type type)
      : AsyncTask(session),
        id_(id),
        location_(std::move(location)),
        identifier_(std::move(identifier)),
        state_(std::move(state)),
        type_(type) {}

  uint64_t GetId() const override { return id_; }
  AsyncTask::Type GetType() const override { return type_; }
  const Location& GetLocation() const override { return location_; }
  const Identifier& GetIdentifier() const override { return identifier_; }
  std::string GetState() const override { return state_; }
  const std::vector<NamedValue>& GetValues() const override { return values_; }
  std::vector<AsyncTask::Ref> GetChildren() const override {
    std::vector<AsyncTask::Ref> ret;
    ret.reserve(children_.size());
    for (const auto& child : children_) {
      ret.push_back(*child);
    }
    return ret;
  }

  void AddChild(std::unique_ptr<AsyncTask> child) { children_.push_back(std::move(child)); }
  void AddNamedValue(std::optional<std::string> name, ExprValue value) {
    values_.push_back({.name = std::move(name), .value = std::move(value)});
  }

 private:
  uint64_t id_;
  Location location_;
  Identifier identifier_;
  std::string state_;
  AsyncTask::Type type_;
  std::vector<NamedValue> values_;
  std::vector<std::unique_ptr<AsyncTask>> children_;
};

bool IsSentinel(uint64_t ptr) { return ptr == 0 || (ptr & kContainerSentinelBit) != 0; }

bool IsRuntimeDispatchFrame(std::string_view name) {
  return debug::StringStartsWith(name, "driver_runtime::") ||
         debug::StringStartsWith(name, "async::internal::") ||
         debug::StringStartsWith(name, "async::Task") || debug::StringStartsWith(name, "fit::") ||
         debug::StringStartsWith(name, "fdf::ChannelRead::CallHandler") ||
         debug::StringStartsWith(name, "fbl::");
}

bool IsDispatcherFrameName(std::string_view name) {
  std::string_view stripped = StripTemplate(name);
  return stripped == "driver_runtime::Dispatcher::DispatchCallback" ||
         stripped == "driver_runtime::Dispatcher::DispatchCallbacks" ||
         stripped == "driver_runtime::Dispatcher::QueueRegisteredCallback";
}

// Extracts the first template argument, Callable, from an instantiation of
// fit::internal::target<Callable, is_inline, is_shared, Allocator, Result, Args...>::invoke.
std::string ExtractCallableName(std::string_view full_name) {
  if (!debug::StringStartsWith(full_name, "fit::internal::target<")) {
    return std::string(full_name);
  }
  size_t start = full_name.find('<');
  if (start == std::string_view::npos) {
    return std::string(full_name);
  }
  start++;
  size_t depth = 0;
  size_t end = start;
  for (; end < full_name.size(); ++end) {
    if (full_name[end] == '<') {
      depth++;
    } else if (full_name[end] == '>') {
      if (depth == 0) {
        break;
      }
      depth--;
    } else if (full_name[end] == ',' && depth == 0) {
      break;
    }
  }
  return std::string(full_name.substr(start, end - start));
}

// Formats a callable or function name into an Identifier, normalizing Clang and
// GCC anonymous lambda type names to end with a {closure} component so
// FormatPrettyComponent in format_name.cc renders lambda symbols cleanly.
Identifier FormatCallableIdentifier(std::string_view callable_name) {
  if (callable_name.ends_with("::operator()")) {
    callable_name.remove_suffix(sizeof("::operator()") - 1);
  }

  size_t lambda_pos = callable_name.rfind("::$_");
  if (lambda_pos == std::string_view::npos) {
    lambda_pos = callable_name.rfind("::{lambda");
  }
  if (lambda_pos == std::string_view::npos) {
    lambda_pos = callable_name.rfind("::(lambda");
  }
  if (lambda_pos == std::string_view::npos) {
    lambda_pos = callable_name.rfind("::(anonymous class)");
  }
  if (lambda_pos == std::string_view::npos) {
    lambda_pos = callable_name.rfind("::(anon class)");
  }
  if (lambda_pos != std::string_view::npos) {
    std::string_view prefix = callable_name.substr(0, lambda_pos);
    if (debug::StringEndsWith(prefix, "()")) {
      prefix.remove_suffix(2);
    }
    Identifier ident;
    if (ExprParser::ParseIdentifier(std::string(prefix), &ident).ok()) {
      ident.AppendComponent(IdentifierComponent("{closure}"));
      return ident;
    }
  }

  if (callable_name.starts_with("(lambda") || callable_name.starts_with("{lambda") ||
      callable_name.starts_with("$_") || callable_name.starts_with("(anonymous class)") ||
      callable_name.starts_with("(anon class)")) {
    return Identifier("{closure}");
  }

  Identifier ident;
  if (ExprParser::ParseIdentifier(std::string(callable_name), &ident).ok()) {
    return ident;
  }
  return Identifier(std::string(callable_name));
}

// Extracts the FileLine from a Clang lambda type name when the lambda call
// operator is inlined into fit::internal::target::invoke.
FileLine ParseLambdaFileLine(std::string_view callable_name) {
  constexpr std::string_view kPrefix1 = "(lambda at ";
  constexpr std::string_view kPrefix2 = "{lambda at ";
  size_t pos = callable_name.find(kPrefix1);
  char close_delim = ')';
  if (pos == std::string_view::npos) {
    pos = callable_name.find(kPrefix2);
    close_delim = '}';
  }
  if (pos == std::string_view::npos) {
    return FileLine();
  }
  pos += kPrefix1.size();
  size_t end = callable_name.find(close_delim, pos);
  if (end == std::string_view::npos) {
    return FileLine();
  }
  std::string_view loc_str = callable_name.substr(pos, end - pos);
  size_t last_colon = loc_str.rfind(':');
  if (last_colon == std::string_view::npos || last_colon == 0) {
    return FileLine();
  }
  size_t prev_colon = loc_str.rfind(':', last_colon - 1);
  if (prev_colon != std::string_view::npos) {
    std::string file(loc_str.substr(0, prev_colon));
    int line = 0;
    if (!file.empty() &&
        fxl::StringToNumberWithError<int>(
            loc_str.substr(prev_colon + 1, last_colon - prev_colon - 1), &line) &&
        line > 0) {
      return FileLine(std::move(file), line);
    }
  }
  std::string file(loc_str.substr(0, last_colon));
  int line = 0;
  if (!file.empty() && fxl::StringToNumberWithError<int>(loc_str.substr(last_colon + 1), &line) &&
      line > 0) {
    return FileLine(std::move(file), line);
  }
  return FileLine();
}

// Extracts the dispatcher name from driver_runtime::Dispatcher::name_.
// The name_ member is an fbl::StringBuffer that stores the active character
// count in length_ and the null-terminated character array in data_.
std::string ExtractDispatcherName(const fxl::RefPtr<EvalContext>& context,
                                  const ExprValue& disp_val) {
  ErrOrValue data_or = ResolveNonstaticMember(context, disp_val, {"name_", "data_"});
  if (data_or.has_error()) {
    return "default";
  }

  const auto& bytes = data_or.value().data().bytes();
  size_t max_len = bytes.size();
  ErrOrValue len_or = ResolveNonstaticMember(context, disp_val, {"name_", "length_"});
  uint64_t reported_len = 0;
  if (len_or.ok() && len_or.value().PromoteTo64(&reported_len).ok()) {
    max_len = std::min<size_t>(reported_len, bytes.size());
  }

  const char* char_ptr = reinterpret_cast<const char*>(bytes.data());
  size_t str_len = strnlen(char_ptr, std::min<size_t>(max_len, 32));
  return std::string(char_ptr, str_len);
}

// Extracts the head_ pointer of an fbl::DoublyLinkedList member on
// driver_runtime::Dispatcher. DoublyLinkedList inherits from
// fbl::internal::DoublyLinkedListBase, whose head_ member points to the first
// node in the list or holds a sentinel with bit 0 set when empty.
uint64_t ExtractQueueHeadAddr(const fxl::RefPtr<EvalContext>& context, const ExprValue& disp_val,
                              const std::string& queue_member_name) {
  ErrOrValue head_or = ResolveNonstaticMember(context, disp_val, {queue_member_name, "head_"});
  uint64_t head_addr = 0;
  if (head_or.ok()) {
    head_or.value().PromoteTo64(&head_addr);
  }
  return head_addr;
}

// Extracts the next_ pointer from a driver_runtime::CallbackRequest node.
// CallbackRequest inherits from fbl::DoublyLinkedListable, which embeds
// dll_node_state_.next_ and dll_node_state_.prev_.
uint64_t ExtractNextCallbackRequestAddr(const fxl::RefPtr<EvalContext>& context,
                                        const ExprValue& req_val) {
  ErrOrValue next_or = ResolveNonstaticMember(context, req_val, {"dll_node_state_", "next_"});
  uint64_t next_addr = 0;
  if (next_or.ok()) {
    next_or.value().PromoteTo64(&next_addr);
  }
  return next_addr;
}

// Extracts the async_task_t* pointer associated with a
// driver_runtime::CallbackRequest. Dispatcher::PostTask stores the
// async_task_t* in async_operation_, which is a std::optional<void*> whose
// libc++ representation uses __engaged_ and __val_.
uint64_t ExtractTaskPointerFromRequest(const fxl::RefPtr<EvalContext>& context,
                                       const ExprValue& req_val) {
  ErrOrValue async_op_or = ResolveNonstaticMember(context, req_val, {"async_operation_"});
  if (async_op_or.ok()) {
    ErrOrValue engaged_or = ResolveNonstaticMember(context, async_op_or.value(), {"__engaged_"});
    ErrOrValue val_or = ResolveNonstaticMember(context, async_op_or.value(), {"__val_"});
    if (engaged_or.ok() && val_or.ok()) {
      uint64_t engaged = 0;
      uint64_t ptr = 0;
      if (engaged_or.value().PromoteTo64(&engaged).ok() && engaged != 0 &&
          val_or.value().PromoteTo64(&ptr).ok() && ptr != 0) {
        return ptr;
      }
    } else {
      uint64_t ptr = 0;
      if (async_op_or.value().PromoteTo64(&ptr).ok() && ptr != 0) {
        return ptr;
      }
    }
  }
  return 0;
}

// Extracts the ops_ vtable pointer from CallbackRequest::callback_, which
// inherits from fit::internal::generic_function_base.
uint64_t ExtractCallbackOpsFromRequest(const fxl::RefPtr<EvalContext>& context,
                                       const ExprValue& req_val) {
  ErrOrValue ops_or = ResolveNonstaticMember(context, req_val, {"callback_", "ops_"});
  uint64_t ops_addr = 0;
  if (ops_or.ok()) {
    ops_or.value().PromoteTo64(&ops_addr);
  }
  return ops_addr;
}

// Finds the DWARF Type for fit::internal::target_ops. Because
// generic_function_base::ops_ is declared as const base_target_ops*, which only
// contains move and destroy, we resolve the derived target_ops struct containing
// invoke either from the static fit::internal::target::ops symbol at ops_addr
// or via EvalContext::FindTypeDefinition.
fxl::RefPtr<Type> ResolveFitTargetOpsType(const fxl::RefPtr<EvalContext>& context,
                                          uint64_t ops_addr) {
  Location ops_loc = context->GetLocationForAddress(ops_addr);
  if (ops_loc.symbol().is_valid()) {
    if (const Value* val_sym = ops_loc.symbol().Get()->As<Value>()) {
      if (const Type* sym_type = val_sym->type().Get()->As<Type>()) {
        fxl::RefPtr<Type> concrete = context->GetConcreteType(sym_type);
        if (concrete && concrete->As<Collection>() && !concrete->is_declaration()) {
          return concrete;
        }
      }
    }
  }
  return context->FindTypeDefinition("fit::internal::target_ops<void>");
}

// Resolves the invoke function pointer from a fit::internal::target_ops vtable
// at ops_addr using ResolvePointer and ResolveNonstaticMember.
void ResolveFitInvokeAddress(const fxl::RefPtr<EvalContext>& context, uint64_t ops_addr,
                             fit::callback<void(const Err&, uint64_t)> cb) {
  fxl::RefPtr<Type> ops_type = ResolveFitTargetOpsType(context, ops_addr);
  if (!ops_type) {
    cb(Err("Missing DWARF type for fit::internal::target_ops."), 0);
    return;
  }

  ResolvePointer(context, ops_addr, std::move(ops_type),
                 [context, cb = std::move(cb)](ErrOrValue ops_val_or) mutable {
                   if (ops_val_or.has_error()) {
                     cb(ops_val_or.err(), 0);
                     return;
                   }
                   ErrOrValue invoke_or =
                       ResolveNonstaticMember(context, ops_val_or.value(), {"invoke"});
                   if (invoke_or.has_error()) {
                     cb(invoke_or.err(), 0);
                     return;
                   }
                   uint64_t invoke_addr = 0;
                   invoke_or.value().PromoteTo64(&invoke_addr);
                   cb(Err(), invoke_addr);
                 });
}

// Walks the UncachedLazySymbol parent chain of a lambda Collection to find its
// enclosing Function symbol. Because UncachedLazySymbol::Get returns a newly
// constructed RefPtr<Symbol> and returns a non-null empty Symbol when invalid,
// callers must check is_valid on the UncachedLazySymbol before calling Get and
// retain the returned RefPtr<Function>.
fxl::RefPtr<Function> FindEnclosingFunction(const Collection* coll) {
  if (!coll) {
    return nullptr;
  }
  constexpr size_t kMaxParentDepth = 16;
  UncachedLazySymbol lazy_p = coll->parent();
  for (size_t depth = 0; depth < kMaxParentDepth && lazy_p.is_valid(); ++depth) {
    fxl::RefPtr<Symbol> p = lazy_p.Get();
    if (!p || p->tag() == DwarfTag::kNone || p->tag() == DwarfTag::kCompileUnit) {
      break;
    }
    if (const Function* fn = p->As<Function>()) {
      return RefPtrTo(fn);
    }
    lazy_p = p->parent();
  }
  return nullptr;
}

// Extracts the Collection symbol for the first template parameter Callable on
// fit::internal::target<Callable, ...>::invoke or its enclosing target struct.
fxl::RefPtr<Collection> FindCallableCollectionFromInvoke(const Function* invoke_func) {
  if (!invoke_func) {
    return nullptr;
  }
  fxl::RefPtr<Symbol> parent_sym;
  const std::vector<LazySymbol>* template_params = nullptr;
  if (!invoke_func->template_params().empty()) {
    template_params = &invoke_func->template_params();
  } else if (invoke_func->parent().is_valid()) {
    parent_sym = invoke_func->parent().Get();
    if (const auto* parent_coll = parent_sym->As<Collection>();
        parent_coll && !parent_coll->template_params().empty()) {
      template_params = &parent_coll->template_params();
    }
  }

  if (template_params && !template_params->empty()) {
    if (const auto* param = (*template_params)[0].Get()->As<TemplateParameter>()) {
      if (param->type()) {
        if (const auto* coll = param->type().Get()->As<Collection>()) {
          return RefPtrTo(coll);
        }
      }
    }
  }
  return nullptr;
}

struct ResolvedCallableTarget {
  Location location;
  Identifier identifier;
};

// Resolves the user callable identifier and source Location from the
// fit::internal::target<Callable, ...>::invoke function symbol.
//
// target::invoke is a static method of the target struct template whose first
// template parameter is the user Callable type, such as a C++ lambda class.
// When the compiler inlines the lambda call operator into target::invoke, we
// recover the user source location and enclosing function name either from the
// lambda location in Callable's type name or from the Collection symbol for
// Callable in target's template_params, including its data_members declaration
// lines and its parent Function.
ResolvedCallableTarget ResolveCallableFromInvokeAddress(const fxl::RefPtr<EvalContext>& context,
                                                        uint64_t invoke_addr) {
  ResolvedCallableTarget result;
  if (invoke_addr == 0) {
    return result;
  }

  Location invoke_loc = context->GetLocationForAddress(invoke_addr);
  if (!invoke_loc.symbol().is_valid()) {
    return result;
  }

  std::string invoke_full_name = invoke_loc.symbol().Get()->GetFullName();
  std::string callable_name = ExtractCallableName(invoke_full_name);
  result.identifier = FormatCallableIdentifier(callable_name);

  FileLine parsed_line = ParseLambdaFileLine(callable_name);
  if (parsed_line.is_valid()) {
    result.location = Location(0, parsed_line, 0, SymbolContext::ForRelativeAddresses());
  }

  if (const Function* invoke_func = invoke_loc.symbol().Get()->As<Function>()) {
    if (fxl::RefPtr<Collection> coll = FindCallableCollectionFromInvoke(invoke_func)) {
      fxl::RefPtr<Function> enclosing_fn = FindEnclosingFunction(coll.get());
      if (!result.location.is_valid()) {
        for (const auto& lazy_m : coll->data_members()) {
          if (const auto* m = lazy_m.Get()->As<DataMember>(); m && m->decl_line().is_valid()) {
            result.location =
                Location(0, m->decl_line(), 0, SymbolContext::ForRelativeAddresses(), coll);
            break;
          }
        }
        if (!result.location.is_valid() && enclosing_fn && enclosing_fn->decl_line().is_valid()) {
          result.location = Location(0, enclosing_fn->decl_line(), 0,
                                     SymbolContext::ForRelativeAddresses(), enclosing_fn);
        }
      } else if (!result.location.symbol().is_valid()) {
        result.location = Location(0, result.location.file_line(), 0,
                                   SymbolContext::ForRelativeAddresses(), coll);
      }

      if (result.identifier.GetFullNameNoQual() == "{closure}" && enclosing_fn) {
        Identifier fn_ident = enclosing_fn->GetIdentifier();
        if (!fn_ident.empty()) {
          fn_ident.AppendComponent(IdentifierComponent("{closure}"));
          result.identifier = std::move(fn_ident);
        }
      }
    }

    if (!result.location.is_valid() && invoke_func->decl_line().is_valid() &&
        !debug::StringContains(invoke_func->decl_line().file(), "function.h")) {
      result.location = Location(0, invoke_func->decl_line(), 0,
                                 SymbolContext::ForRelativeAddresses(), RefPtrTo(invoke_func));
    }
  }

  if (!result.location.is_valid()) {
    result.location = std::move(invoke_loc);
  }
  return result;
}

// Builds the two-node AsyncTask subtree with a kTask parent and kFunction child
// expected by FormatAsyncTask for a single queued, delayed, or waiting task.
std::unique_ptr<AsyncTask> BuildTaskSubtree(Session* session, uint64_t task_id,
                                            Location func_location, Identifier func_identifier,
                                            const std::string& task_state,
                                            std::optional<uint64_t> deadline = std::nullopt) {
  auto task_node =
      std::make_unique<FdfDispatcherTask>(session, task_id, Location(), Identifier("Task"),
                                          "id = " + to_hex_string(task_id), AsyncTask::Type::kTask);

  auto func_node = std::make_unique<FdfDispatcherTask>(session, 0, std::move(func_location),
                                                       std::move(func_identifier), task_state,
                                                       AsyncTask::Type::kFunction);

  if (task_state == "Delayed" && deadline.has_value()) {
    uint64_t dl = *deadline;
    if (dl != 0 && dl != 0x8000000000000000ULL && dl != 0x7fffffffffffffffULL &&
        dl != 0xffffffffffffffffULL) {
      func_node->AddNamedValue("deadline", ExprValue(dl));
    }
  }

  task_node->AddChild(std::move(func_node));
  return task_node;
}

// Resolves the fit::closure or fit::function stored inside a C++ libasync task
// wrapper such as async::internal::RetainedTask, async::TaskClosure, or
// async::Task, and builds the resulting AsyncTask node.
void InspectAsyncTaskClosureWrapper(Session* session, const fxl::RefPtr<EvalContext>& context,
                                    uint64_t task_ptr, const Location& handler_loc,
                                    const std::string& handler_name, const std::string& task_state,
                                    uint64_t deadline,
                                    fit::callback<void(std::unique_ptr<AsyncTask>)> cb) {
  bool is_retained_task = debug::StringContains(handler_name, "RetainedTask::Handler");
  std::string_view wrapper_type_name = "async::Task";
  if (is_retained_task) {
    wrapper_type_name = "async::internal::RetainedTask";
  } else if (debug::StringContains(handler_name, "TaskClosure::CallHandler")) {
    wrapper_type_name = "async::TaskClosure";
  }
  std::string fit_member_name = is_retained_task ? "handler" : "handler_";

  fxl::RefPtr<Type> wrapper_type;
  if (const Function* handler_func = handler_loc.symbol().Get()->As<Function>();
      handler_func && handler_func->parent().is_valid()) {
    fxl::RefPtr<Symbol> parent_sym = handler_func->parent().Get();
    if (const Type* parent_type = parent_sym->As<Type>()) {
      wrapper_type = context->GetConcreteType(parent_type);
    }
  }
  if (!wrapper_type || wrapper_type->is_declaration()) {
    wrapper_type = context->FindTypeDefinition(wrapper_type_name);
  }
  if (!wrapper_type) {
    cb(BuildTaskSubtree(session, task_ptr, handler_loc, FormatCallableIdentifier(handler_name),
                        task_state, deadline));
    return;
  }

  ResolvePointer(
      context, task_ptr, std::move(wrapper_type),
      [session, context, task_ptr, handler_loc, handler_name, fit_member_name, task_state, deadline,
       cb = std::move(cb)](ErrOrValue wrapper_or) mutable {
        if (wrapper_or.has_error()) {
          cb(BuildTaskSubtree(session, task_ptr, handler_loc,
                              FormatCallableIdentifier(handler_name), task_state, deadline));
          return;
        }

        ErrOrValue ops_or =
            ResolveNonstaticMember(context, wrapper_or.value(), {fit_member_name, "ops_"});
        uint64_t closure_ops = 0;
        if (ops_or.ok()) {
          ops_or.value().PromoteTo64(&closure_ops);
        }
        if (closure_ops == 0) {
          cb(BuildTaskSubtree(session, task_ptr, handler_loc,
                              FormatCallableIdentifier(handler_name), task_state, deadline));
          return;
        }

        ResolveFitInvokeAddress(
            context, closure_ops,
            [session, context, task_ptr, task_state, deadline, cb = std::move(cb)](
                const Err& err, uint64_t invoke_addr) mutable {
              ResolvedCallableTarget resolved;
              if (!err.has_error() && invoke_addr != 0) {
                resolved = ResolveCallableFromInvokeAddress(context, invoke_addr);
              }
              if (resolved.identifier.components().empty()) {
                resolved.identifier = Identifier("Task");
              }
              cb(BuildTaskSubtree(session, task_ptr, std::move(resolved.location),
                                  std::move(resolved.identifier), task_state, deadline));
            });
      });
}

// Inspects an async_task_t by resolving its handler and deadline members via
// ResolveNonstaticMember.
void InspectAsyncTaskStruct(Session* session, const fxl::RefPtr<EvalContext>& context,
                            uint64_t task_ptr, std::string task_state,
                            fit::callback<void(std::unique_ptr<AsyncTask>)> cb) {
  ResolvePointer(
      context, task_ptr, context->FindTypeDefinition("async_task"),
      [session, context, task_ptr, task_state = std::move(task_state),
       cb = std::move(cb)](ErrOrValue task_val_or) mutable {
        if (task_val_or.has_error()) {
          cb(BuildTaskSubtree(session, task_ptr, Location(), Identifier("UnknownTask"),
                              task_state));
          return;
        }

        const ExprValue& task_val = task_val_or.value();
        ErrOrValue handler_or = ResolveNonstaticMember(context, task_val, {"handler"});
        ErrOrValue deadline_or = ResolveNonstaticMember(context, task_val, {"deadline"});
        uint64_t handler_addr = 0;
        if (handler_or.ok()) {
          handler_or.value().PromoteTo64(&handler_addr);
        }
        uint64_t deadline = 0;
        if (deadline_or.ok()) {
          deadline_or.value().PromoteTo64(&deadline);
        }

        Location handler_loc = context->GetLocationForAddress(handler_addr);
        std::string handler_name;
        if (handler_loc.symbol().is_valid()) {
          handler_name = handler_loc.symbol().Get()->GetFullName();
        }

        if (debug::StringContains(handler_name, "RetainedTask::Handler") ||
            debug::StringContains(handler_name, "CallHandler")) {
          InspectAsyncTaskClosureWrapper(session, context, task_ptr, handler_loc, handler_name,
                                         task_state, deadline, std::move(cb));
          return;
        }

        Identifier user_ident =
            handler_name.empty() ? Identifier("Task") : FormatCallableIdentifier(handler_name);
        cb(BuildTaskSubtree(session, task_ptr, std::move(handler_loc), std::move(user_ident),
                            task_state, deadline));
      });
}

// Inspects a direct driver_runtime::Callback on CallbackRequest::callback_ when
// the CallbackRequest does not wrap an async_task_t, such as a registered
// fdf::ChannelRead callback.
void InspectDirectCallbackRequest(Session* session, const fxl::RefPtr<EvalContext>& context,
                                  uint64_t req_addr, uint64_t callback_ops, std::string task_state,
                                  fit::callback<void(std::unique_ptr<AsyncTask>)> cb) {
  ResolveFitInvokeAddress(context, callback_ops,
                          [session, context, req_addr, task_state = std::move(task_state),
                           cb = std::move(cb)](const Err& err, uint64_t invoke_addr) mutable {
                            ResolvedCallableTarget resolved;
                            if (!err.has_error() && invoke_addr != 0) {
                              resolved = ResolveCallableFromInvokeAddress(context, invoke_addr);
                            }
                            if (resolved.identifier.components().empty()) {
                              resolved.identifier = Identifier("Callback");
                            }
                            cb(BuildTaskSubtree(session, req_addr, std::move(resolved.location),
                                                std::move(resolved.identifier), task_state));
                          });
}

// Resolves a single driver_runtime::CallbackRequest at req_addr and dispatches
// to InspectAsyncTaskStruct or InspectDirectCallbackRequest.
void FetchCallbackRequest(Session* session, const fxl::RefPtr<EvalContext>& context,
                          fxl::RefPtr<Type> req_type, uint64_t req_addr, std::string task_state,
                          fit::callback<void(std::unique_ptr<AsyncTask>)> cb) {
  ResolvePointer(context, req_addr, std::move(req_type),
                 [session, context, req_addr, task_state = std::move(task_state),
                  cb = std::move(cb)](ErrOrValue req_val_or) mutable {
                   if (req_val_or.has_error()) {
                     cb(nullptr);
                     return;
                   }

                   const ExprValue& req_val = req_val_or.value();
                   uint64_t task_ptr = ExtractTaskPointerFromRequest(context, req_val);
                   if (task_ptr != 0) {
                     InspectAsyncTaskStruct(session, context, task_ptr, std::move(task_state),
                                            std::move(cb));
                     return;
                   }

                   uint64_t callback_ops = ExtractCallbackOpsFromRequest(context, req_val);
                   if (callback_ops != 0) {
                     InspectDirectCallbackRequest(session, context, req_addr, callback_ops,
                                                  std::move(task_state), std::move(cb));
                     return;
                   }

                   cb(nullptr);
                 });
}

// Walks an fbl::DoublyLinkedList of driver_runtime::CallbackRequest nodes
// starting at current_addr by resolving each node as a CallbackRequest and
// reading its dll_node_state_.next_ member via ResolveNonstaticMember.
void CollectQueueNodeAddresses(const fxl::RefPtr<EvalContext>& context, fxl::RefPtr<Type> req_type,
                               uint64_t current_addr, std::vector<uint64_t> acc,
                               std::set<uint64_t> visited,
                               fit::callback<void(std::vector<uint64_t>)> cb) {
  if (IsSentinel(current_addr) || !visited.insert(current_addr).second ||
      acc.size() >= kMaxQueueNodes) {
    cb(std::move(acc));
    return;
  }

  ResolvePointer(context, current_addr, req_type,
                 [context, req_type, current_addr, acc = std::move(acc),
                  visited = std::move(visited), cb = std::move(cb)](ErrOrValue req_val_or) mutable {
                   if (req_val_or.has_error()) {
                     cb(std::move(acc));
                     return;
                   }
                   acc.push_back(current_addr);
                   uint64_t next_addr = ExtractNextCallbackRequestAddr(context, req_val_or.value());
                   if (IsSentinel(next_addr)) {
                     cb(std::move(acc));
                     return;
                   }
                   CollectQueueNodeAddresses(context, std::move(req_type), next_addr,
                                             std::move(acc), std::move(visited), std::move(cb));
                 });
}

// Traverses a single fbl::DoublyLinkedList queue on driver_runtime::Dispatcher
// starting from its resolved head_addr pointer.
void TraverseQueueFromHead(Session* session, const fxl::RefPtr<EvalContext>& context,
                           uint64_t head_addr, std::string task_state,
                           fit::callback<void(std::vector<std::unique_ptr<AsyncTask>>)> cb) {
  if (IsSentinel(head_addr)) {
    cb({});
    return;
  }

  fxl::RefPtr<Type> req_type = context->FindTypeDefinition("driver_runtime::CallbackRequest");
  CollectQueueNodeAddresses(
      context, req_type, head_addr, {}, {},
      [session, context, req_type, task_state = std::move(task_state),
       cb = std::move(cb)](const std::vector<uint64_t>& node_addrs) mutable {
        if (node_addrs.empty()) {
          cb({});
          return;
        }

        auto joiner = fxl::MakeRefCounted<JoinCallbacks<std::unique_ptr<AsyncTask>>>();
        for (uint64_t node_addr : node_addrs) {
          FetchCallbackRequest(session, context, req_type, node_addr, task_state,
                               joiner->AddCallback());
        }

        joiner->Ready([cb = std::move(cb)](std::vector<std::unique_ptr<AsyncTask>> tasks) mutable {
          std::vector<std::unique_ptr<AsyncTask>> non_null_tasks;
          for (auto& t : tasks) {
            if (t) {
              non_null_tasks.push_back(std::move(t));
            }
          }
          cb(std::move(non_null_tasks));
        });
      });
}

// Reconstructs the currently executing task on the thread stack when stopped
// inside a callback dispatched by driver_runtime::Dispatcher::DispatchCallback
// or driver_runtime::Dispatcher::DispatchCallbacks.
std::unique_ptr<AsyncTask> CollectActiveStackTasks(Session* session,
                                                   const fxl::WeakPtr<Thread>& thread,
                                                   uint64_t dispatcher_addr) {
  if (!thread) {
    return nullptr;
  }

  const Stack& stack = thread->GetStack();
  if (stack.empty()) {
    return nullptr;
  }

  size_t disp_idx = stack.size();
  for (size_t i = 0; i < stack.size(); ++i) {
    const Frame* f = stack[i];
    if (f && f->GetLocation().symbol().is_valid()) {
      std::string name = f->GetLocation().symbol().Get()->GetFullName();
      if (IsDispatcherFrameName(name)) {
        disp_idx = i;
        break;
      }
    }
  }

  if (disp_idx == stack.size() || disp_idx == 0) {
    return nullptr;
  }

  std::vector<const Frame*> user_frames;
  for (size_t i = 0; i < disp_idx; ++i) {
    const Frame* f = stack[i];
    if (f && f->GetLocation().symbol().is_valid()) {
      std::string name = f->GetLocation().symbol().Get()->GetFullName();
      if (!IsRuntimeDispatchFrame(name)) {
        user_frames.push_back(f);
      }
    }
  }

  if (user_frames.empty()) {
    return nullptr;
  }

  const Frame* entry_frame = user_frames.back();
  std::string entry_name = entry_frame->GetLocation().symbol().Get()->GetFullName();
  Identifier entry_ident = FormatCallableIdentifier(entry_name);

  uint64_t task_id = dispatcher_addr + 1;
  auto task_node =
      std::make_unique<FdfDispatcherTask>(session, task_id, Location(), Identifier("Task"),
                                          "id = " + to_hex_string(task_id), AsyncTask::Type::kTask);

  auto entry_func_node = std::make_unique<FdfDispatcherTask>(
      session, 0, entry_frame->GetLocation(), entry_ident, "Running", AsyncTask::Type::kFunction);

  const Frame* top_frame = user_frames.front();
  if (top_frame != entry_frame) {
    Identifier top_ident = top_frame->GetLocation().symbol().Get()->GetIdentifier();
    auto top_func_node = std::make_unique<FdfDispatcherTask>(
        session, 0, top_frame->GetLocation(), top_ident, "Running", AsyncTask::Type::kFunction);
    entry_func_node->AddChild(std::move(top_func_node));
  }

  task_node->AddChild(std::move(entry_func_node));
  return task_node;
}

// Asynchronous helper that locates the active driver_runtime::Dispatcher on
// the stack and resolves it into a concrete ExprValue.
//
// On arm64-balanced builds, driver_runtime::Dispatcher::DispatchCallback
// receives this in the caller-saved register x0, which is clobbered once the
// callback is invoked. Its caller frame on the stack,
// driver_runtime::Dispatcher::DispatchCallbacks, preserves this in a
// callee-saved register. Iterating through all candidate contexts allows
// EvalExpression to fall through clobbered frames to caller frames where this
// is still available.
class DispatcherLocator : public fxl::RefCountedThreadSafe<DispatcherLocator> {
 public:
  using Callback = fit::callback<void(const Err&, ExprValue, fxl::RefPtr<EvalContext>)>;

  DispatcherLocator(const fxl::RefPtr<EvalContext>& context, fxl::WeakPtr<Thread> thread,
                    Callback cb)
      : orig_context_(context), thread_(std::move(thread)), cb_(std::move(cb)) {
    if (thread_) {
      const Stack& stack = thread_->GetStack();
      for (size_t i = 0; i < stack.size(); ++i) {
        const Frame* s_frame = stack[i];
        if (s_frame && s_frame->GetLocation().symbol().is_valid()) {
          std::string s_name = s_frame->GetLocation().symbol().Get()->GetFullName();
          if (IsDispatcherFrameName(s_name)) {
            if (auto s_context = s_frame->GetEvalContext()) {
              candidate_contexts_.push_back(std::move(s_context));
            }
          }
        }
      }
    }
    if (orig_context_) {
      bool already_present = false;
      for (const auto& c : candidate_contexts_) {
        if (c.get() == orig_context_.get()) {
          already_present = true;
          break;
        }
      }
      if (!already_present) {
        candidate_contexts_.push_back(orig_context_);
      }
    }
  }

  void Start() { TryNextThis(0); }

 private:
  void CompleteWithDispatcherAddr(const fxl::RefPtr<EvalContext>& ctx, uint64_t disp_addr,
                                  fit::callback<void()> on_fail) {
    if (IsSentinel(disp_addr)) {
      on_fail();
      return;
    }
    ResolvePointer(ctx, disp_addr, ctx->FindTypeDefinition("driver_runtime::Dispatcher"),
                   [this_ref = fxl::RefPtr<DispatcherLocator>(this), ctx,
                    on_fail = std::move(on_fail)](ErrOrValue disp_val_or) mutable {
                     if (disp_val_or.ok() && !disp_val_or.value().data().empty()) {
                       if (this_ref->cb_) {
                         auto cb = std::move(this_ref->cb_);
                         cb(Err(), disp_val_or.take_value(), ctx);
                       }
                       return;
                     }
                     on_fail();
                   });
  }

  static bool IsDispatcherPointerType(const fxl::RefPtr<EvalContext>& context, const Type* type) {
    fxl::RefPtr<Type> pointed_to;
    if (GetPointedToType(context, type, &pointed_to).has_error() || !pointed_to) {
      return false;
    }
    fxl::RefPtr<Type> concrete = context->GetConcreteType(pointed_to.get());
    if (!concrete) {
      return false;
    }
    std::string_view full_name = StripTemplate(concrete->GetFullName());
    if (full_name == "driver_runtime::Dispatcher" || full_name == "::driver_runtime::Dispatcher") {
      return true;
    }
    fxl::RefPtr<Type> disp_type = context->FindTypeDefinition("driver_runtime::Dispatcher");
    return disp_type && concrete.get() == disp_type.get();
  }

  void TryNextThis(size_t idx) {
    if (idx >= candidate_contexts_.size()) {
      TryLoopFallback();
      return;
    }

    auto cur_context = candidate_contexts_[idx];
    EvalExpression(
        "this", cur_context, false,
        [this_ref = fxl::RefPtr<DispatcherLocator>(this), cur_context, idx](ErrOrValue val) {
          uint64_t addr = 0;
          if (!val.ok() || !IsDispatcherPointerType(cur_context, val.value().type()) ||
              val.value().PromoteTo64(&addr).has_error()) {
            this_ref->TryNextThis(idx + 1);
            return;
          }
          this_ref->CompleteWithDispatcherAddr(cur_context, addr,
                                               [this_ref, idx] { this_ref->TryNextThis(idx + 1); });
        });
  }

  // When a driver runtime worker thread is idle inside async_loop_run or
  // async_loop_run_once, no driver_runtime::Dispatcher method is on the stack.
  // Instead, the async_loop holds a driver_runtime::EventWaiter in its
  // wait_list.
  //
  // EventWaiter inherits from AsyncLoopOwnedEventHandler<EventWaiter>, whose
  // first member wait_ begins with async_wait_t. Consequently, loop->wait_list
  // points to the driver_runtime::EventWaiter, and
  // EventWaiter::dispatcher_ref_.ptr_ points to the driver_runtime::Dispatcher.
  void TryLoopFallback() {
    std::vector<fxl::RefPtr<EvalContext>> loop_contexts;
    if (orig_context_) {
      loop_contexts.push_back(orig_context_);
    }
    if (thread_) {
      const Stack& stack = thread_->GetStack();
      for (size_t i = 0; i < stack.size(); ++i) {
        const Frame* s_frame = stack[i];
        if (s_frame && s_frame->GetLocation().symbol().is_valid()) {
          std::string s_name = s_frame->GetLocation().symbol().Get()->GetFullName();
          std::string_view stripped = StripTemplate(s_name);
          if (stripped == "async_loop_run_once" || stripped == "async_loop_run" ||
              debug::StringContains(s_name, "async_loop_run")) {
            if (auto s_context = s_frame->GetEvalContext()) {
              bool already = false;
              for (const auto& c : loop_contexts) {
                if (c.get() == s_context.get()) {
                  already = true;
                  break;
                }
              }
              if (!already) {
                loop_contexts.push_back(std::move(s_context));
              }
            }
          }
        }
      }
    }

    TryNextLoop(std::move(loop_contexts), 0);
  }

  void TryNextLoop(std::vector<fxl::RefPtr<EvalContext>> loop_contexts, size_t idx) {
    if (idx >= loop_contexts.size()) {
      TryDispatcherFallback();
      return;
    }

    auto cur_context = loop_contexts[idx];
    EvalExpression(
        "loop", cur_context, false,
        [this_ref = fxl::RefPtr<DispatcherLocator>(this), loop_contexts = std::move(loop_contexts),
         cur_context, idx](ErrOrValue loop_ptr_or) mutable {
          uint64_t loop_addr = 0;
          if (!loop_ptr_or.ok() || loop_ptr_or.value().PromoteTo64(&loop_addr).has_error() ||
              IsSentinel(loop_addr)) {
            this_ref->TryNextLoop(std::move(loop_contexts), idx + 1);
            return;
          }
          ResolvePointer(cur_context, loop_addr, cur_context->FindTypeDefinition("async_loop"),
                         [this_ref, loop_contexts = std::move(loop_contexts), cur_context,
                          idx](ErrOrValue loop_val_or) mutable {
                           if (loop_val_or.has_error()) {
                             this_ref->TryNextLoop(std::move(loop_contexts), idx + 1);
                             return;
                           }
                           this_ref->ResolveEventWaiterFromLoop(cur_context, loop_val_or.value(),
                                                                std::move(loop_contexts), idx);
                         });
        });
  }

  void ResolveEventWaiterFromLoop(const fxl::RefPtr<EvalContext>& cur_context,
                                  const ExprValue& loop_val,
                                  std::vector<fxl::RefPtr<EvalContext>> loop_contexts, size_t idx) {
    // In async_loop, wait_list is a circular list_node_t where the most
    // recently registered async_wait_t is at the head of the list in
    // wait_list.prev or wait_list.next, and an empty list points back to
    // wait_list itself.
    ErrOrValue wait_list_or = ResolveNonstaticMember(cur_context, loop_val, {"wait_list"});
    if (wait_list_or.has_error()) {
      TryNextLoop(std::move(loop_contexts), idx + 1);
      return;
    }
    uint64_t wait_list_addr = wait_list_or.value().source().address();

    uint64_t wait_node_addr = 0;
    ErrOrValue wait_node_or = ResolveNonstaticMember(cur_context, wait_list_or.value(), {"prev"});
    if (wait_node_or.has_error() || wait_node_or.value().PromoteTo64(&wait_node_addr).has_error() ||
        wait_node_addr == 0 || (wait_list_addr != 0 && wait_node_addr == wait_list_addr)) {
      wait_node_or = ResolveNonstaticMember(cur_context, wait_list_or.value(), {"next"});
      if (wait_node_or.ok()) {
        wait_node_or.value().PromoteTo64(&wait_node_addr);
      }
    }
    if (IsSentinel(wait_node_addr) || (wait_list_addr != 0 && wait_node_addr == wait_list_addr)) {
      TryNextLoop(std::move(loop_contexts), idx + 1);
      return;
    }

    ResolvePointer(
        cur_context, wait_node_addr, cur_context->FindTypeDefinition("driver_runtime::EventWaiter"),
        [this_ref = fxl::RefPtr<DispatcherLocator>(this), loop_contexts = std::move(loop_contexts),
         cur_context, idx](ErrOrValue waiter_or) mutable {
          if (waiter_or.has_error()) {
            this_ref->TryNextLoop(std::move(loop_contexts), idx + 1);
            return;
          }
          ErrOrValue disp_ptr_or =
              ResolveNonstaticMember(cur_context, waiter_or.value(), {"dispatcher_ref_", "ptr_"});
          uint64_t disp_addr = 0;
          if (disp_ptr_or.has_error() || disp_ptr_or.value().PromoteTo64(&disp_addr).has_error()) {
            this_ref->TryNextLoop(std::move(loop_contexts), idx + 1);
            return;
          }
          this_ref->CompleteWithDispatcherAddr(
              cur_context, disp_addr,
              [this_ref, loop_contexts = std::move(loop_contexts), idx]() mutable {
                this_ref->TryNextLoop(std::move(loop_contexts), idx + 1);
              });
        });
  }

  void TryDispatcherFallback() {
    auto cur_context = orig_context_;
    if (!cur_context && !candidate_contexts_.empty()) {
      cur_context = candidate_contexts_[0];
    }
    if (!cur_context) {
      if (cb_) {
        auto cb = std::move(cb_);
        cb(Err("No matching driver_runtime::Dispatcher found."), ExprValue(), nullptr);
      }
      return;
    }

    EvalExpression(
        "dispatcher", cur_context, false,
        [this_ref = fxl::RefPtr<DispatcherLocator>(this), cur_context](ErrOrValue disp_val) {
          uint64_t disp_addr = 0;
          if (disp_val.ok() && disp_val.value().PromoteTo64(&disp_addr).ok()) {
            this_ref->CompleteWithDispatcherAddr(cur_context, disp_addr, [this_ref] {
              if (this_ref->cb_) {
                auto cb = std::move(this_ref->cb_);
                cb(Err("No matching driver_runtime::Dispatcher found."), ExprValue(), nullptr);
              }
            });
            return;
          }
          if (this_ref->cb_) {
            auto cb = std::move(this_ref->cb_);
            cb(Err("No matching driver_runtime::Dispatcher found."), ExprValue(), nullptr);
          }
        });
  }

  fxl::RefPtr<EvalContext> orig_context_;
  fxl::WeakPtr<Thread> thread_;
  Callback cb_;
  std::vector<fxl::RefPtr<EvalContext>> candidate_contexts_;
};

void LocateDispatcher(const fxl::RefPtr<EvalContext>& context, fxl::WeakPtr<Thread> thread,
                      DispatcherLocator::Callback cb) {
  auto locator = fxl::MakeRefCounted<DispatcherLocator>(context, std::move(thread), std::move(cb));
  locator->Start();
}

}  // namespace

FdfDispatcherTaskProvider::FdfDispatcherTaskProvider() = default;
FdfDispatcherTaskProvider::~FdfDispatcherTaskProvider() = default;

bool FdfDispatcherTaskProvider::CanHandle(Frame* frame) const {
  if (!frame) {
    return false;
  }

  if (frame->GetLocation().symbol().is_valid()) {
    std::string func_name(frame->GetLocation().symbol().Get()->GetFullName());
    std::string_view stripped = StripTemplate(func_name);

    if (IsDispatcherFrameName(stripped) ||
        stripped == "driver_runtime::DispatcherCoordinator::TestingRun" ||
        stripped == "driver_runtime::DispatcherCoordinator::TestingRunUntilIdle" ||
        stripped == "fdf_testing::DriverRuntime::Run") {
      return true;
    }

    if ((stripped == "async_loop_run_once" || stripped == "async_loop_run") && frame->GetThread()) {
      const std::string& thread_name = frame->GetThread()->GetName();
      if (debug::StringStartsWith(thread_name, "fdf-dispatcher-thread-")) {
        return true;
      }

      const Stack& stack = frame->GetThread()->GetStack();
      for (size_t i = 0; i < stack.size(); ++i) {
        const Frame* s_frame = stack[i];
        if (s_frame && s_frame->GetLocation().symbol().is_valid()) {
          std::string s_name = s_frame->GetLocation().symbol().Get()->GetFullName();
          if (debug::StringStartsWith(s_name, "driver_runtime::") ||
              debug::StringStartsWith(s_name, "fdf::") ||
              debug::StringStartsWith(s_name, "fdf_testing::")) {
            return true;
          }
        }
      }
    }
  }

  return false;
}

void FdfDispatcherTaskProvider::GetTasks(
    Frame* frame, fit::callback<void(const Err&, std::vector<std::unique_ptr<AsyncTask>>)> cb) {
  if (!frame) {
    debug::MessageLoop::Current()->PostTask(FROM_HERE, [cb = std::move(cb)]() mutable {
      cb(Err("No frame available to inspect async tasks."), {});
    });
    return;
  }

  Session* session = frame->session();
  auto context = frame->GetEvalContext();
  auto thread = frame->GetThread() ? frame->GetThread()->GetWeakPtr() : fxl::WeakPtr<Thread>();

  LocateDispatcher(
      context, thread,
      [session, context, thread, cb = std::move(cb)](
          const Err& err, const ExprValue& disp_val,
          const fxl::RefPtr<EvalContext>& disp_context) mutable {
        uint64_t dispatcher_addr = disp_val.source().address();
        if (err.has_error() || dispatcher_addr == 0) {
          cb(err.has_error() ? err : Err("Could not locate active driver_runtime::Dispatcher."),
             {});
          return;
        }

        auto active_context = disp_context ? disp_context : context;
        auto active_task = CollectActiveStackTasks(session, thread, dispatcher_addr);
        std::string dispatcher_name = ExtractDispatcherName(active_context, disp_val);

        uint64_t callback_queue_head =
            ExtractQueueHeadAddr(active_context, disp_val, "callback_queue_");
        uint64_t delayed_tasks_head =
            ExtractQueueHeadAddr(active_context, disp_val, "delayed_tasks_");
        uint64_t always_on_delayed_head =
            ExtractQueueHeadAddr(active_context, disp_val, "always_on_delayed_tasks_");
        uint64_t registered_callbacks_head =
            ExtractQueueHeadAddr(active_context, disp_val, "registered_callbacks_");

        auto queue_joiner =
            fxl::MakeRefCounted<JoinCallbacks<std::vector<std::unique_ptr<AsyncTask>>>>();

        TraverseQueueFromHead(session, active_context, callback_queue_head, "Pending",
                              queue_joiner->AddCallback());
        TraverseQueueFromHead(session, active_context, delayed_tasks_head, "Delayed",
                              queue_joiner->AddCallback());
        TraverseQueueFromHead(session, active_context, always_on_delayed_head, "Delayed",
                              queue_joiner->AddCallback());
        TraverseQueueFromHead(session, active_context, registered_callbacks_head, "Waiting",
                              queue_joiner->AddCallback());

        queue_joiner->Ready(
            [session, dispatcher_addr, dispatcher_name = std::move(dispatcher_name),
             active_task = std::move(active_task), cb = std::move(cb)](
                std::vector<std::vector<std::unique_ptr<AsyncTask>>> queue_results) mutable {
              auto root = std::make_unique<FdfDispatcherTask>(
                  session, dispatcher_addr, Location(), FormatCallableIdentifier("fdf::Dispatcher"),
                  dispatcher_name, AsyncTask::Type::kScope);

              std::vector<std::unique_ptr<AsyncTask>> all_children;
              if (active_task) {
                all_children.push_back(std::move(active_task));
              }

              for (auto& queue_tasks : queue_results) {
                for (auto& task : queue_tasks) {
                  if (task) {
                    all_children.push_back(std::move(task));
                  }
                }
              }

              for (auto& child : all_children) {
                root->AddChild(std::move(child));
              }

              std::vector<std::unique_ptr<AsyncTask>> result;
              result.push_back(std::move(root));
              cb(Err(), std::move(result));
            });
      });
}

}  // namespace zxdb
