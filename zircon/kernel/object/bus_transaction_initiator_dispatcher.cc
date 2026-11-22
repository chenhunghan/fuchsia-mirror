// Copyright 2017 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "object/bus_transaction_initiator_dispatcher.h"

#include <lib/object-constants.h>

BusTransactionInitiatorDispatcher::BusTransactionInitiatorDispatcher(const void* bti)
    : Dispatcher(0u) {
  DISPATCHER_VERIFY_OFFSET(BusTransactionInitiatorDispatcher,
                           kBusTransactionInitiatorDispatcherStateOffset);
  rust_bus_transaction_initiator_dispatcher_state_init(&opaque_storage_, this, bti);
}

IMPLEMENT_DISPATCHER_RUST_STATE(BusTransactionInitiatorDispatcher,
                                rust_bus_transaction_initiator_dispatcher_state_get_lock,
                                rust_bus_transaction_initiator_dispatcher_state_destroy)

void BusTransactionInitiatorDispatcher::on_zero_handles() {
  rust_bus_transaction_initiator_dispatcher_on_zero_handles(this);
}
