// Copyright 2021 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <object/dispatcher.h>
#include <object/pager_dispatcher.h>
#include <object/pager_proxy.h>

PagerProxy::PagerProxy(PagerDispatcher* dispatcher, fbl::RefPtr<PortDispatcher> port, uint64_t key,
                       uint32_t options) {
  DISPATCHER_VERIFY_OFFSET(PagerProxy, kPagerProxyStateOffset);
  rust_pager_proxy_state_init(&opaque_storage_, dispatcher, fbl::ExportToRawPtr(&port), key,
                              options);
}

PagerProxy::~PagerProxy() { rust_pager_proxy_state_destroy(&opaque_storage_); }
