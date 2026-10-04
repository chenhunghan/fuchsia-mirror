// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package netstack

import (
	"syscall/zx/fidl"

	syslog "go.fuchsia.dev/fuchsia/src/lib/syslog/go"

	"fidl/fuchsia/net/interfaces/admin"
	"fidl/fuchsia/net/stack"

	"gvisor.dev/gvisor/pkg/atomicbitops"
	"gvisor.dev/gvisor/pkg/tcpip"
)

var _ stack.StackWithCtx = (*stackImpl)(nil)

type stackImpl struct {
	ns *Netstack
}

func (ni *stackImpl) SetDhcpClientEnabled(ctx_ fidl.Context, id uint64, enable bool) (stack.StackSetDhcpClientEnabledResult, error) {
	var r stack.StackSetDhcpClientEnabledResult

	nicInfo, ok := ni.ns.stack.NICInfo()[tcpip.NICID(id)]
	if !ok {
		return stack.StackSetDhcpClientEnabledResultWithErr(stack.ErrorNotFound), nil
	}

	ifState := nicInfo.Context.(*ifState)
	ifState.setDHCPStatus(nicInfo.Name, enable)

	r.SetResponse(stack.StackSetDhcpClientEnabledResponse{})
	return r, nil
}

func (ni *stackImpl) BridgeInterfaces(_ fidl.Context, interfaces []uint64, bridge admin.ControlWithCtxInterfaceRequest) error {
	_ = syslog.Infof("received request to bridge %v", interfaces)
	nics := make([]tcpip.NICID, len(interfaces))
	for i, n := range interfaces {
		nics[i] = tcpip.NICID(n)
	}
	ifs, err := ni.ns.Bridge(nics)
	if err != nil {
		_ = syslog.Warnf("failed to bridge interfaces %s", err)
		proxy := admin.ControlEventProxy{
			Channel: bridge.Channel,
		}
		if err := proxy.OnInterfaceRemoved(admin.InterfaceRemovedReasonBadPort); err != nil {
			_ = syslog.Warnf("failed to write terminal event: %s", err)
		}
		_ = bridge.Close()
		return nil
	}
	ifs.addAdminConnection(bridge, true /* strong */)
	return nil
}

var _ stack.LogWithCtx = (*logImpl)(nil)

type logImpl struct {
	logPackets *atomicbitops.Uint32
}

func (li *logImpl) SetLogPackets(_ fidl.Context, enabled bool) error {
	var val uint32
	if enabled {
		val = 1
	}
	li.logPackets.Store(val)
	syslog.VLogTf(syslog.DebugVerbosity, "fuchsia_net_stack", "SetLogPackets: %t", enabled)
	return nil
}
