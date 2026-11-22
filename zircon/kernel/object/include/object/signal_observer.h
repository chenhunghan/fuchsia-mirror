// Copyright 2020 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_SIGNAL_OBSERVER_H_
#define ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_SIGNAL_OBSERVER_H_

#include <lib/object-constants.h>
#include <stdint.h>
#include <zircon/types.h>

#include <fbl/intrusive_double_list.h>

class SignalObserver;

extern "C" {
void rust_signal_observer_on_match(SignalObserver* observer, zx_signals_t signals,
                                   void* queue_to_own);
void rust_signal_observer_on_cancel(SignalObserver* observer, zx_signals_t signals);
bool rust_signal_observer_matches_key(const SignalObserver* observer, const void* port,
                                      uint64_t key);
}

// C++ layout mirror of Rust's `SignalObserver` (see `zircon/kernel/object/signal_observer.rs`
// for the canonical documentation and concrete observer dispatch). Retained only while C++
// `Dispatcher` manages its intrusive `observers_` list directly.
class SignalObserver : public fbl::DoublyLinkedListable<SignalObserver*> {
 public:
  SignalObserver() = delete;
  SignalObserver(const SignalObserver&) = delete;
  SignalObserver& operator=(const SignalObserver&) = delete;

  const void* handle() const { return handle_; }

  // |queue_to_own| is an opaque pointer to a Rust OwnedWaitQueue, or nullptr.
  void OnMatch(zx_signals_t signals, void* queue_to_own = nullptr) {
    rust_signal_observer_on_match(this, signals, queue_to_own);
  }

  void OnCancel(zx_signals_t signals) { rust_signal_observer_on_cancel(this, signals); }

  bool MatchesKey(const void* port, uint64_t key) const {
    return rust_signal_observer_matches_key(this, port, key);
  }

 private:
  static void VerifyLayout() {
    static_assert(sizeof(SignalObserver) == kSignalObserverSize, "SignalObserver size mismatch");
    static_assert(alignof(SignalObserver) == kSignalObserverAlign, "SignalObserver align mismatch");
    static_assert(kSignalObserverNodeOffset == 0,
                  "SignalObserver list node must be the first subobject");
    static_assert(sizeof(fbl::DoublyLinkedListable<SignalObserver*>) ==
                      kSignalObserverTriggeringSignalsOffset - kSignalObserverNodeOffset,
                  "SignalObserver list node size mismatch");
#pragma clang diagnostic push
#pragma clang diagnostic ignored "-Winvalid-offsetof"
    static_assert(__builtin_offsetof(SignalObserver, triggering_signals_) ==
                      kSignalObserverTriggeringSignalsOffset,
                  "SignalObserver::triggering_signals_ offset mismatch");
    static_assert(__builtin_offsetof(SignalObserver, options_) == kSignalObserverOptionsOffset,
                  "SignalObserver::options_ offset mismatch");
    static_assert(__builtin_offsetof(SignalObserver, handle_) == kSignalObserverHandleOffset,
                  "SignalObserver::handle_ offset mismatch");
#pragma clang diagnostic pop
  }

  // Dispatcher state, guarded by Dispatcher's lock.
  friend class Dispatcher;

  zx_signals_t triggering_signals_{0};
  [[maybe_unused]] const uint32_t options_{0};
  const void* handle_{nullptr};
};

#endif  // ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_SIGNAL_OBSERVER_H_
