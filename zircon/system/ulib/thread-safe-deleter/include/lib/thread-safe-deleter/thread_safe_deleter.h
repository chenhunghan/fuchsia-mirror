// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef LIB_THREAD_SAFE_DELETER_THREAD_SAFE_DELETER_H_
#define LIB_THREAD_SAFE_DELETER_THREAD_SAFE_DELETER_H_

#include <lib/closure-queue/closure_queue.h>
#include <zircon/assert.h>

// ThreadSafeDeleter
//
// This "holder" class is for holding instances of classes which must only be used on a single
// thread, but which are safe to curry to other threads (and back) between usages.  This also means
// the held instance must be safe to delete on any thread after it has been moved out.
//
// This class holds an instance of a moveable type, and ensures that the not-moved-out instance gets
// deleted on the correct thread, even if the destructor of the holder is called on the wrong
// thread.
//
// If the Held type (or dereferenced with ->) has a PrepareForAsyncDelete, any synchronous deletion
// of a not-moved-out ThreadSafeDeleter called from outside the sequence of the closure_queue will
// automatically call held.PrepareForAsyncDelete or held->PrepareForAsyncDelete. If the client code
// needs to ensure PrepareForAsyncDelete is called sooner, see EnsurePreparedForAsyncDelete. The
// PrepareForAsyncDelete is never called from within the sequence of the closure_queue. If the
// ThreadSafeDeleter is destructed (or moved into) while already running on the sequence of the
// closure_queue, PrepareForAsyncDelete is never called and Held is destructed synchronously.
//
// The ThreadSafeDeleter does not provide any synchronization beyond the posting to the deletion
// thread described in this comment block. For example calls to EnsurePreparedForAsyncDelete and the
// move constructor and so on must be serialized by the caller.
//
// One use case:
//
// HLCPP FIDL callbacks are affinitized to the FIDL thread on which they're created.  They must only
// be deleted on the FIDL-handling thread they were created on.  Sometimes in normal operation it's
// convenient to curry a FIDL callback to another thread, then back to the FIDL thread to get called
// and deleted.  However, when shutting down, the currying can be cut short and the lambda currying
// the callback can be deleted on the wrong thread.
template <typename Held>
class ThreadSafeDeleter {
 public:
  // closure_queue - a ClosureQueue that'll out-last the ThreadSafeDeleter, which can be used to
  // run the held's destructor on the correct thread.
  ThreadSafeDeleter(ClosureQueue* closure_queue, Held&& held);

  ~ThreadSafeDeleter();

  // move-only, no copy
  ThreadSafeDeleter(ThreadSafeDeleter&& other);
  ThreadSafeDeleter& operator=(ThreadSafeDeleter&& other);
  ThreadSafeDeleter(const ThreadSafeDeleter& other) = delete;
  ThreadSafeDeleter& operator=(const ThreadSafeDeleter& other) = delete;

  [[nodiscard]]
  Held& held();

  // This method can be used to ensure that Held::PrepareForAsyncDelete (or
  // held->PrepareForAsyncDelete) has been called (if not already previously called). This method
  // can be useful in situations where the calling code is manually moving and/or posting the
  // ThreadSafeDeleter to the sequence of the closure_queue (whether via the closure_queue or not)
  // and wants Held to prepare before posting, or similar.
  //
  // Calling EnsurePreparedForAsyncDelete while already on the sequence of the closure_queue,
  // whether via something posted to the closure_queue or not, is not permitted, and will fail a
  // ZX_ASSERT. In some cases the client code may need to explicitly check
  // closure_queue->IsSynchronized() as part of deciding whether to call
  // EnsurePreparedForAsyncDelete.
  //
  // This method is idempotent. The caller must serialize calls to this method.
  void EnsurePreparedForAsyncDelete();

 private:
  void DeleteHeld();

  ClosureQueue* closure_queue_ = nullptr;
  // If ~ThreadSafeDeleter runs on the wrong thread, then the Held will be moved out, curried
  // over to the correct thread, and ~Held run there.
  Held held_;
  bool is_moved_out_ = false;
  bool prepare_if_present_called_ = false;
};

template <typename Held>
ThreadSafeDeleter<Held>::ThreadSafeDeleter(ClosureQueue* closure_queue, Held&& held)
    : closure_queue_(closure_queue), held_(std::move(held)) {
  ZX_DEBUG_ASSERT(closure_queue_);
  ZX_DEBUG_ASSERT(!is_moved_out_);
}

template <typename Held>
ThreadSafeDeleter<Held>::~ThreadSafeDeleter() {
  DeleteHeld();
}

template <typename Held>
ThreadSafeDeleter<Held>::ThreadSafeDeleter(ThreadSafeDeleter&& other)
    : closure_queue_(other.closure_queue_),
      held_(std::move(other.held_)),
      prepare_if_present_called_(other.prepare_if_present_called_) {
  ZX_DEBUG_ASSERT(!other.is_moved_out_);
  other.is_moved_out_ = true;
  ZX_DEBUG_ASSERT(!is_moved_out_);
}

template <typename Held>
ThreadSafeDeleter<Held>& ThreadSafeDeleter<Held>::operator=(ThreadSafeDeleter&& other) {
  ZX_DEBUG_ASSERT(!other.is_moved_out_);
  // Prevent this for now since we don't need it.  Not fundamentally invalid, but also not great
  // practice for the caller to do this, so let's not.
  ZX_DEBUG_ASSERT(!is_moved_out_);
  DeleteHeld();
  closure_queue_ = other.closure_queue_;
  held_ = std::move(other.held_);
  prepare_if_present_called_ = other.prepare_if_present_called_;
  other.is_moved_out_ = true;
  return *this;
}

template <typename Held>
Held& ThreadSafeDeleter<Held>::held() {
  ZX_DEBUG_ASSERT(!is_moved_out_);
  return held_;
}

template <typename T>
void CallPrepareIfPresent(T& obj) {
  if constexpr (requires { obj.PrepareForAsyncDelete(); }) {
    obj.PrepareForAsyncDelete();
  } else if constexpr (requires { obj->PrepareForAsyncDelete(); }) {
    if (obj) {
      obj->PrepareForAsyncDelete();
    }
  }
}

template <typename Held>
void ThreadSafeDeleter<Held>::EnsurePreparedForAsyncDelete() {
  if (is_moved_out_) {
    return;
  }
  if (!prepare_if_present_called_) {
    // Caller must not call EnsurePreparedForAsyncDelete while running on the sequence of the
    // closure_queue_.
    ZX_ASSERT(!closure_queue_->IsSynchronized());
    prepare_if_present_called_ = true;
    CallPrepareIfPresent(held_);
  }
}

template <typename Held>
void ThreadSafeDeleter<Held>::DeleteHeld() {
  if (is_moved_out_) {
    return;
  }
  if (!closure_queue_->IsSynchronized()) {
    EnsurePreparedForAsyncDelete();
    closure_queue_->Enqueue([held = std::move(held_)] {
      // ~held, on correct thread
    });
  } else {
    // Given current callers, this block is unnecessary because all callers will shortly delete
    // held_ on the current thread anyway, but to make it easier to reason about what DeleteHeld
    // does, go ahead and move held_ out and delete the moved instance here. This way DeleteHeld
    // moves out held_ and ensures deletion whether IsSynchronized() or not.
    Held held = std::move(held_);
    // ~held
  }
}

#endif  // LIB_THREAD_SAFE_DELETER_THREAD_SAFE_DELETER_H_
