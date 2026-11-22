// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//go:build !linux

package main

func isVirtualizedCasfsARI(_ string) bool {
	return false
}
