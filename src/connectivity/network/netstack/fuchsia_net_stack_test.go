// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package netstack

import (
	"context"
	"testing"

	"fidl/fuchsia/net/stack"
)

func AssertNoError(t *testing.T, err error) {
	t.Helper()
	if err != nil {
		t.Errorf("Received unexpected error:\n%+v", err)
	}
}

func TestSetDhcpClientEnabled(t *testing.T) {
	t.Run("bad NIC", func(t *testing.T) {
		ns, _ := newNetstack(t, netstackTestOptions{})
		stackServiceImpl := stackImpl{ns: ns}

		result, err := stackServiceImpl.SetDhcpClientEnabled(context.Background(), 1234, true)
		if err != nil {
			t.Fatalf("stackServiceImpl.StartDhcpClient(...) = %s", err)
		}
		if got, want := result.Which(), stack.I_stackSetDhcpClientEnabledResultTag(stack.StackSetDhcpClientEnabledResultErr); got != want {
			t.Fatalf("got result.Which() = %d, want = %d", got, want)
		}
		if got, want := result.Err, stack.ErrorNotFound; got != want {
			t.Fatalf("got result.Err = %s, want = %s", got, want)
		}
	})

	t.Run("good NIC", func(t *testing.T) {
		ns, _ := newNetstack(t, netstackTestOptions{})
		ifs1 := addNoopEndpoint(t, ns, "")
		t.Cleanup(ifs1.RemoveByUser)

		stackServiceImpl := stackImpl{ns: ns}

		result, err := stackServiceImpl.SetDhcpClientEnabled(context.Background(), uint64(ifs1.nicid), true)
		if err != nil {
			t.Fatalf("stackServiceImpl.StartDhcpClient(...) = %s", err)
		}
		if got, want := result.Which(), stack.I_stackSetDhcpClientEnabledResultTag(stack.StackSetDhcpClientEnabledResultResponse); got != want {
			t.Fatalf("got result.Which() = %d, want = %d", got, want)
		}
	})
}
