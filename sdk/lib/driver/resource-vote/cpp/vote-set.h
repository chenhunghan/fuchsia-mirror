// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef LIB_DRIVER_RESOURCE_VOTE_CPP_VOTE_SET_H_
#define LIB_DRIVER_RESOURCE_VOTE_CPP_VOTE_SET_H_

#include <lib/fit/function.h>
#include <lib/fpromise/promise.h>
#include <lib/inspect/cpp/inspect.h>
#include <lib/zx/clock.h>
#include <lib/zx/time.h>
#include <zircon/assert.h>

#include <cstdint>
#include <limits>
#include <map>
#include <optional>
#include <string>
#include <string_view>
#include <utility>
#include <vector>

// A library for tracking per-connection votes on a shared hardware resource (a clock, a power
// domain, a regulator, ...) that is turned on on behalf of several consumers.
//
// `VoteSet<Request>` is a synchronous state machine. It does no I/O; the caller applies hardware
// effects. Every state-changing operation is two-phase:
//
//   auto plan = votes.PrepareIncrement(vote, true);
//   if (plan.changed()) {
//     zx_status_t status = TurnHardwareOn();
//     if (status != ZX_OK) {
//       votes.Abandon(std::move(plan));  // State unchanged.
//       return zx::error(status);
//     }
//   }
//   votes.Commit(std::move(plan));
//
// A plan is invalidated by any intervening `Commit()` or `Abandon()` on the same set.
//
// Each consumer connection owns a `Vote`, which is attributed to the consumer's node (its
// `NodeKey`, e.g. the child node / service instance name the connection arrived through). When
// the connection closes, the caller calls `PrepareClose()`:
//
//  * `CloseBehavior::kOrphanOnClose` (driver consumers): if the vote has a request that
//    contributes to the aggregate, it is kept as an *orphan* for its NodeKey and the aggregate is
//    unchanged. One WARN log is emitted. The orphan is removed when a new vote for the same
//    NodeKey sets a request (reclaim) or when `PrepareNodeStarted()` is committed for that
//    NodeKey. A request contributes iff `merge({request})` is not `std::nullopt`; a request that
//    contributes nothing (e.g. `false` in a `BoolVoteSet`) is simply dropped on close.
//  * `CloseBehavior::kReleaseOnClose` (debug/tool connections): equivalent to clearing the vote.
//    If applying the resulting plan fails, call `Abandon()`. Because the connection is gone, the
//    vote cannot stay live: it is instead retained as an orphan for its NodeKey (with one WARN
//    log), and its release is retried by the next reclaim or `PrepareNodeStarted()` for that
//    NodeKey. Callers should log the hardware failure itself at ERROR, and should give
//    `kReleaseOnClose` votes a NodeKey on which a later reclaim or node start can happen.
//
// Named holds (e.g. "boot", "kernel") contribute to the aggregate independently of any vote.
//
// Threading: `VoteSet` is not thread-safe. It must only be used from a single synchronized
// dispatcher (or otherwise externally synchronized).

namespace fdf_resource_vote {

// Determines what happens to a vote's request when its connection closes.
enum class CloseBehavior : uint8_t {
  // The request is kept as an orphan attributed to the vote's NodeKey (driver consumers).
  kOrphanOnClose,
  // The request is released (debug/tool connections that have no node to rebind).
  kReleaseOnClose,
};

// The kind of an entry that contributes to the aggregate.
enum class EntryKind : uint8_t {
  // A vote whose connection is open.
  kLive,
  // A vote whose connection closed while it held a request.
  kOrphan,
  // A named system hold.
  kHold,
};

// Returns "live", "orphan" or "hold".
std::string_view EntryKindToString(EntryKind kind);

// Severity of a log message emitted by a `VoteSet`.
enum class LogSeverity : uint8_t {
  kWarning,
  kError,
};

// Receives log messages emitted by a `VoteSet`. By default messages go to the driver's global
// logger (`fdf::Logger`), if one is set.
using LogSink = fit::function<void(LogSeverity severity, const std::string& message)>;

namespace internal {
uint64_t NextId();
void DefaultLog(LogSeverity severity, const std::string& message);
}  // namespace internal

// Identifies one consumer connection's vote. Votes are not tied to a particular `VoteSet`; they
// are lightweight, move-only identities. A vote must not be used after a `PrepareClose()` plan for
// it has been committed or abandoned.
class Vote {
 public:
  Vote(std::string node_key, CloseBehavior close_behavior)
      : id_(internal::NextId()), node_key_(std::move(node_key)), close_behavior_(close_behavior) {}

  Vote(Vote&& other) noexcept
      : id_(std::exchange(other.id_, 0)),
        node_key_(std::move(other.node_key_)),
        close_behavior_(other.close_behavior_) {}
  Vote& operator=(Vote&& other) noexcept {
    id_ = std::exchange(other.id_, 0);
    node_key_ = std::move(other.node_key_);
    close_behavior_ = other.close_behavior_;
    return *this;
  }
  Vote(const Vote&) = delete;
  Vote& operator=(const Vote&) = delete;

  uint64_t id() const { return id_; }
  const std::string& node_key() const { return node_key_; }
  CloseBehavior close_behavior() const { return close_behavior_; }

 private:
  uint64_t id_;
  std::string node_key_;
  CloseBehavior close_behavior_;
};

// A snapshot of one entry contributing to the aggregate.
template <typename Request>
struct Entry {
  EntryKind kind;
  // The NodeKey for live and orphaned votes, the hold name for holds.
  std::string key;
  Request request;
  // Phase-1 counted votes: the number of outstanding increments. 1 for `PrepareSet()` votes and
  // holds.
  uint32_t count;
  // When the entry was created or last changed kind or request.
  zx::time_monotonic since;
};

template <typename Request>
class VoteSet;

// The result of a `Prepare*()` call: the aggregate before and after the operation. Must be
// passed to `Commit()` (the caller applied `new_aggregate()` to hardware, or nothing changed) or
// `Abandon()` (applying failed). Dropping a plan is equivalent to `Abandon()`, except for close
// plans, which must be committed or abandoned.
template <typename Request>
class [[nodiscard]] Plan {
 public:
  // A moved-from plan is rejected by `Commit()` and `Abandon()`.
  Plan(Plan&& other) noexcept
      : owner_(std::exchange(other.owner_, nullptr)),
        generation_(other.generation_),
        old_aggregate_(std::move(other.old_aggregate_)),
        new_aggregate_(std::move(other.new_aggregate_)),
        next_(std::move(other.next_)),
        orphaned_id_(std::exchange(other.orphaned_id_, std::nullopt)),
        closing_id_(std::exchange(other.closing_id_, std::nullopt)) {}
  Plan& operator=(Plan&& other) noexcept {
    owner_ = std::exchange(other.owner_, nullptr);
    generation_ = other.generation_;
    old_aggregate_ = std::move(other.old_aggregate_);
    new_aggregate_ = std::move(other.new_aggregate_);
    next_ = std::move(other.next_);
    orphaned_id_ = std::exchange(other.orphaned_id_, std::nullopt);
    closing_id_ = std::exchange(other.closing_id_, std::nullopt);
    return *this;
  }
  Plan(const Plan&) = delete;
  Plan& operator=(const Plan&) = delete;

  const std::optional<Request>& old_aggregate() const { return old_aggregate_; }
  const std::optional<Request>& new_aggregate() const { return new_aggregate_; }

  // True if the caller must apply `new_aggregate()` to hardware before committing.
  bool changed() const { return old_aggregate_ != new_aggregate_; }

 private:
  friend class VoteSet<Request>;
  using Entries = std::map<uint64_t, Entry<Request>>;

  Plan(const VoteSet<Request>* owner, uint64_t generation, std::optional<Request> old_aggregate,
       std::optional<Request> new_aggregate, Entries next)
      : owner_(owner),
        generation_(generation),
        old_aggregate_(std::move(old_aggregate)),
        new_aggregate_(std::move(new_aggregate)),
        next_(std::move(next)) {}

  const VoteSet<Request>* owner_;
  uint64_t generation_;
  std::optional<Request> old_aggregate_;
  std::optional<Request> new_aggregate_;
  // The complete entry state after the operation.
  Entries next_;
  // Set if committing this plan turns the vote with this id into an orphan.
  std::optional<uint64_t> orphaned_id_;
  // Set if this is a close plan for the vote with this id.
  std::optional<uint64_t> closing_id_;
};

// Tracks votes for a resource and computes their aggregate. See the comment at the top of this
// file.
//
// `Request` must be copyable and equality comparable.
template <typename Request>
class VoteSet {
 public:
  // Merges the requests of all contributing entries into the aggregate. Called with a non-empty
  // list. Returning `std::nullopt` means "nothing requested" (off).
  using Merge = fit::function<std::optional<Request>(const std::vector<Request>& requests)>;
  // Formats a request for logs and inspect.
  using Formatter = fit::function<std::string(const Request& request)>;

  VoteSet(std::string resource_name, Merge merge, Formatter formatter)
      : resource_name_(std::move(resource_name)),
        merge_(std::move(merge)),
        formatter_(std::move(formatter)),
        log_sink_(internal::DefaultLog) {}

  VoteSet(const VoteSet&) = delete;
  VoteSet& operator=(const VoteSet&) = delete;
  // Not movable: outstanding plans refer to the set by address.
  VoteSet(VoteSet&&) = delete;
  VoteSet& operator=(VoteSet&&) = delete;

  const std::string& resource_name() const { return resource_name_; }

  // The current merged request of all contributing entries.
  const std::optional<Request>& aggregate() const { return aggregate_; }

  // Idempotently records `request` as `vote`'s request (count 1). Atomically removes all orphans
  // with the same NodeKey (reclaim).
  Plan<Request> PrepareSet(const Vote& vote, Request request) {
    Entries next = entries_;
    Reclaim(next, vote.node_key());
    UpsertLive(next, vote, std::move(request), /*increment=*/false);
    return MakePlan(std::move(next));
  }

  // Removes `vote`'s request. No-op if it has none.
  Plan<Request> PrepareClear(const Vote& vote) {
    Entries next = entries_;
    EraseLive(next, vote);
    return MakePlan(std::move(next));
  }

  // Phase-1 counted vote: increments `vote`'s count and records `request` as its request.
  // Atomically removes all orphans with the same NodeKey (reclaim).
  Plan<Request> PrepareIncrement(const Vote& vote, Request request) {
    Entries next = entries_;
    Reclaim(next, vote.node_key());
    UpsertLive(next, vote, std::move(request), /*increment=*/true);
    return MakePlan(std::move(next));
  }

  // Phase-1 counted vote: decrements `vote`'s count, saturating at 0. The vote's request is
  // removed when the count reaches 0.
  Plan<Request> PrepareDecrement(const Vote& vote) {
    Entries next = entries_;
    auto it = next.find(vote.id());
    if (it != next.end() && it->second.kind == EntryKind::kLive) {
      if (it->second.count <= 1) {
        next.erase(it);
      } else {
        it->second.count--;
      }
    }
    return MakePlan(std::move(next));
  }

  // Called when `vote`'s connection closed. See `CloseBehavior`. Close cannot fail: the plan must
  // be committed, or abandoned if applying a changed `kReleaseOnClose` plan failed.
  Plan<Request> PrepareClose(const Vote& vote) {
    Entries next = entries_;
    std::optional<uint64_t> orphaned_id;
    auto it = next.find(vote.id());
    if (it != next.end() && it->second.kind == EntryKind::kLive) {
      if (vote.close_behavior() == CloseBehavior::kOrphanOnClose &&
          Contributes(it->second.request)) {
        it->second.kind = EntryKind::kOrphan;
        it->second.since = zx::clock::get_monotonic();
        orphaned_id = vote.id();
      } else {
        next.erase(it);
      }
    }
    Plan<Request> plan = MakePlan(std::move(next));
    plan.orphaned_id_ = orphaned_id;
    plan.closing_id_ = vote.id();
    return plan;
  }

  // Called when a driver started on node `node_key`. Removes all remaining orphans for it.
  Plan<Request> PrepareNodeStarted(std::string_view node_key) {
    Entries next = entries_;
    Reclaim(next, node_key);
    return MakePlan(std::move(next));
  }

  // Adds the named hold `name` with `request`, replacing the request of an existing hold with the
  // same name.
  Plan<Request> PrepareAddHold(std::string name, Request request) {
    Entries next = entries_;
    zx::time_monotonic now = zx::clock::get_monotonic();
    for (auto& [id, entry] : next) {
      if (entry.kind == EntryKind::kHold && entry.key == name) {
        if (!(entry.request == request)) {
          entry.request = std::move(request);
          entry.since = now;
        }
        return MakePlan(std::move(next));
      }
    }
    next.emplace(internal::NextId(), Entry<Request>{.kind = EntryKind::kHold,
                                                    .key = std::move(name),
                                                    .request = std::move(request),
                                                    .count = 1,
                                                    .since = now});
    return MakePlan(std::move(next));
  }

  // Releases the named hold `name`. No-op if there is no such hold.
  Plan<Request> PrepareReleaseHold(std::string_view name) {
    Entries next = entries_;
    for (auto it = next.begin(); it != next.end(); ++it) {
      if (it->second.kind == EntryKind::kHold && it->second.key == name) {
        next.erase(it);
        break;
      }
    }
    return MakePlan(std::move(next));
  }

  // Applies `plan`. The caller must have applied `plan.new_aggregate()` to hardware if
  // `plan.changed()`.
  void Commit(Plan<Request> plan) {
    if (!CheckPlan(plan)) {
      return;
    }
    if (plan.changed()) {
      transition_count_++;
    }
    entries_ = std::move(plan.next_);
    aggregate_ = std::move(plan.new_aggregate_);
    generation_++;
    if (plan.orphaned_id_.has_value()) {
      LogOrphan(*plan.orphaned_id_, OrphanReason::kClosed);
    }
  }

  // Discards `plan` because applying it to hardware failed. Vote state is unchanged, except for
  // close plans: the closing vote's request (if any) is retained as an orphan for its NodeKey so
  // that it is retried on the next relevant event.
  void Abandon(Plan<Request> plan) {
    if (!CheckPlan(plan)) {
      return;
    }
    generation_++;
    if (!plan.closing_id_.has_value()) {
      return;
    }
    auto it = entries_.find(*plan.closing_id_);
    if (it == entries_.end() || it->second.kind != EntryKind::kLive) {
      return;
    }
    if (!Contributes(it->second.request)) {
      // Nothing to hold; removing it does not change the aggregate.
      entries_.erase(it);
      return;
    }
    it->second.kind = EntryKind::kOrphan;
    it->second.since = zx::clock::get_monotonic();
    LogOrphan(it->first, OrphanReason::kReleaseFailed);
  }

  // Returns a snapshot of all entries contributing to the aggregate, in creation order.
  std::vector<Entry<Request>> entries() const {
    std::vector<Entry<Request>> result;
    result.reserve(entries_.size());
    for (const auto& [id, entry] : entries_) {
      result.push_back(entry);
    }
    return result;
  }

  // The number of committed plans that changed the aggregate.
  uint64_t transition_count() const { return transition_count_; }

  // The number of orphaned entries currently retained.
  size_t orphan_count() const {
    size_t count = 0;
    for (const auto& [id, entry] : entries_) {
      if (entry.kind == EntryKind::kOrphan) {
        count++;
      }
    }
    return count;
  }

  // The total number of orphan events since creation.
  uint64_t orphaned_total() const { return orphaned_total_; }

  // Publishes state lazily on demand under `node`, which is typically a child created for this
  // resource. The `inspect::Inspector` owning `node` must serve its lazy callbacks on the same
  // synchronized dispatcher as this `VoteSet` (as `fdf::DriverContext::CreateInspector()` does).
  void AttachInspect(inspect::Node node) {
    inspect_.reset();
    inspect::LazyNode values = node.CreateLazyValues("votes", [this] { return PopulateInspect(); });
    inspect_ = InspectState{.root = std::move(node), .values = std::move(values)};
  }

  // Replaces the log sink. Intended for tests.
  void SetLogSinkForTesting(LogSink sink) { log_sink_ = std::move(sink); }

 private:
  using Entries = typename Plan<Request>::Entries;

  struct InspectState {
    inspect::Node root;
    // Declared after `root` so `~InspectState()` destroys `values` (synchronizing with any
    // in-flight lazy callback) before `root`.
    inspect::LazyNode values;
  };

  static void Reclaim(Entries& entries, std::string_view node_key) {
    for (auto it = entries.begin(); it != entries.end();) {
      if (it->second.kind == EntryKind::kOrphan && it->second.key == node_key) {
        it = entries.erase(it);
      } else {
        ++it;
      }
    }
  }

  static void UpsertLive(Entries& entries, const Vote& vote, Request request, bool increment) {
    zx::time_monotonic now = zx::clock::get_monotonic();
    auto it = entries.find(vote.id());
    if (it == entries.end()) {
      entries.emplace(vote.id(), Entry<Request>{.kind = EntryKind::kLive,
                                                .key = vote.node_key(),
                                                .request = std::move(request),
                                                .count = 1,
                                                .since = now});
      return;
    }
    Entry<Request>& entry = it->second;
    ZX_DEBUG_ASSERT_MSG(entry.kind == EntryKind::kLive, "vote used after close");
    if (entry.kind != EntryKind::kLive) {
      return;
    }
    if (increment) {
      if (entry.count < std::numeric_limits<uint32_t>::max()) {
        entry.count++;
      }
    } else {
      entry.count = 1;
    }
    if (!(entry.request == request)) {
      entry.request = std::move(request);
      entry.since = now;
    }
  }

  static void EraseLive(Entries& entries, const Vote& vote) {
    auto it = entries.find(vote.id());
    if (it != entries.end() && it->second.kind == EntryKind::kLive) {
      entries.erase(it);
    }
  }

  std::optional<Request> ComputeAggregate(const Entries& entries) const {
    if (entries.empty()) {
      return std::nullopt;
    }
    std::vector<Request> requests;
    requests.reserve(entries.size());
    for (const auto& [id, entry] : entries) {
      requests.push_back(entry.request);
    }
    return merge_(requests);
  }

  Plan<Request> MakePlan(Entries next) const {
    std::optional<Request> new_aggregate = ComputeAggregate(next);
    return Plan<Request>(this, generation_, aggregate_, std::move(new_aggregate), std::move(next));
  }

  bool CheckPlan(const Plan<Request>& plan) {
    ZX_DEBUG_ASSERT_MSG(plan.owner_ == this, "plan from a different VoteSet or moved-from");
    ZX_DEBUG_ASSERT_MSG(plan.generation_ == generation_, "stale plan");
    if (plan.owner_ != this || plan.generation_ != generation_) {
      log_sink_(LogSeverity::kError, resource_name_ + ": ignoring stale or foreign vote plan");
      return false;
    }
    return true;
  }

  enum class OrphanReason : uint8_t {
    // An `kOrphanOnClose` vote's connection closed.
    kClosed,
    // Releasing a `kReleaseOnClose` vote on connection close failed.
    kReleaseFailed,
  };

  // Whether `request` contributes anything to the aggregate on its own.
  bool Contributes(const Request& request) const {
    return merge_(std::vector<Request>{request}).has_value();
  }

  void LogOrphan(uint64_t id, OrphanReason reason) {
    orphaned_total_++;
    const Entry<Request>& entry = entries_.at(id);
    std::string prefix = "Vote on resource '" + resource_name_ + "' from node '" + entry.key + "' ";
    switch (reason) {
      case OrphanReason::kClosed:
        log_sink_(LogSeverity::kWarning, prefix + "orphaned by connection close; holding " +
                                             formatter_(entry.request) + " until the node rebinds");
        break;
      case OrphanReason::kReleaseFailed:
        log_sink_(LogSeverity::kWarning, prefix +
                                             "could not be released on connection close; "
                                             "retaining " +
                                             formatter_(entry.request) +
                                             " as an orphan until reclaimed or the node starts");
        break;
    }
  }

  fpromise::promise<inspect::Inspector> PopulateInspect() const {
    inspect::Inspector inspector;
    inspect::Node& root = inspector.GetRoot();
    root.RecordString("resource", resource_name_);
    root.RecordString("aggregate", aggregate_.has_value() ? formatter_(*aggregate_) : "none");
    root.RecordUint("transition_count", transition_count_);
    root.RecordUint("orphan_count", orphan_count());
    root.RecordUint("orphaned_total", orphaned_total_);
    root.RecordChild("entries", [this](inspect::Node& entries) {
      size_t index = 0;
      for (const auto& [id, entry] : entries_) {
        entries.RecordChild(std::to_string(index++), [this, &entry](inspect::Node& child) {
          child.RecordString("kind", std::string(EntryKindToString(entry.kind)));
          child.RecordString("key", entry.key);
          child.RecordString("request", formatter_(entry.request));
          child.RecordUint("count", entry.count);
          child.RecordInt("since", entry.since.get());
        });
      }
    });
    return fpromise::make_ok_promise(std::move(inspector));
  }

  std::string resource_name_;
  Merge merge_;
  Formatter formatter_;
  LogSink log_sink_;

  Entries entries_;
  std::optional<Request> aggregate_;
  uint64_t generation_ = 0;
  uint64_t transition_count_ = 0;
  uint64_t orphaned_total_ = 0;
  // Declared last so `~VoteSet()` destroys any registered `LazyNode` before other fields.
  std::optional<InspectState> inspect_;
};

// Merge function for boolean votes: `true` if any request is `true`, `std::nullopt` otherwise.
std::optional<bool> MergeAnyTrue(const std::vector<bool>& requests);

// A `VoteSet` for on/off resources. The aggregate is `true` when any entry votes `true`.
//
// For idempotent on/off votes use `PrepareSet(vote, true)` / `PrepareClear(vote)`.
// `PrepareSet(vote, false)` records a vote that does not contribute; it is dropped (not orphaned)
// when its connection closes.
class BoolVoteSet : public VoteSet<bool> {
 public:
  explicit BoolVoteSet(std::string resource_name);

  // Phase-1 counted `Enable()`: increments `vote`'s count.
  Plan<bool> PrepareEnable(const Vote& vote) { return PrepareIncrement(vote, true); }

  // Phase-1 counted `Disable()`: decrements `vote`'s count, saturating at 0.
  Plan<bool> PrepareDisable(const Vote& vote) { return PrepareDecrement(vote); }

  // Whether the aggregate is "on".
  bool enabled() const { return aggregate().value_or(false); }
};

}  // namespace fdf_resource_vote

#endif  // LIB_DRIVER_RESOURCE_VOTE_CPP_VOTE_SET_H_
