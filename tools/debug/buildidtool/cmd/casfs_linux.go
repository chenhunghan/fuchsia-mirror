// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//go:build linux

package main

import "syscall"

// casfsARIXattrs are the extended attribute names exposed by casfs (ABFS) on
// virtualized RBE artifacts. Querying them via getxattr executes purely in
// memory in the kernel module without triggering remote blob hydration.
var casfsARIXattrs = []string{
	"casfs.ari_size",
	"user.casfs.ari_size",
}

// isVirtualizedCasfsARI reports whether path is a casfs-virtualized RBE
// artifact (ARI stub). It never opens the file and therefore never triggers
// on-demand hydration from RBE.
func isVirtualizedCasfsARI(path string) bool {
	for _, attr := range casfsARIXattrs {
		if sz, err := syscall.Getxattr(path, attr, nil); err == nil && sz > 0 {
			return true
		}
	}
	return false
}
