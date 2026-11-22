// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package main

import (
	"bytes"
	"debug/elf"
	"encoding/binary"
	"encoding/hex"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

// writeSyntheticELF writes a minimal valid 64-bit little-endian ELF file with
// a single PT_NOTE program header carrying a GNU build ID note.
func writeSyntheticELF(t *testing.T, path string, buildID []byte) {
	t.Helper()
	const (
		ehdrSize = 64
		phdrSize = 56
		noteOff  = ehdrSize + phdrSize
	)

	var note bytes.Buffer
	name := []byte("GNU\x00")
	_ = binary.Write(&note, binary.LittleEndian, uint32(len(name)))
	_ = binary.Write(&note, binary.LittleEndian, uint32(len(buildID)))
	_ = binary.Write(&note, binary.LittleEndian, uint32(elflibNTGnuBuildID))
	note.Write(name)
	note.Write(buildID)
	for note.Len()%4 != 0 {
		note.WriteByte(0)
	}

	var buf bytes.Buffer
	ehdr := elf.Header64{
		Ident: [elf.EI_NIDENT]byte{
			0x7f, 'E', 'L', 'F',
			byte(elf.ELFCLASS64),
			byte(elf.ELFDATA2LSB),
			byte(elf.EV_CURRENT),
		},
		Type:      uint16(elf.ET_EXEC),
		Machine:   uint16(elf.EM_X86_64),
		Version:   uint32(elf.EV_CURRENT),
		Phoff:     ehdrSize,
		Ehsize:    ehdrSize,
		Phentsize: phdrSize,
		Phnum:     1,
	}
	if err := binary.Write(&buf, binary.LittleEndian, ehdr); err != nil {
		t.Fatalf("writing ELF header: %v", err)
	}
	phdr := elf.Prog64{
		Type:   uint32(elf.PT_NOTE),
		Flags:  uint32(elf.PF_R),
		Off:    noteOff,
		Filesz: uint64(note.Len()),
		Memsz:  uint64(note.Len()),
		Align:  4,
	}
	if err := binary.Write(&buf, binary.LittleEndian, phdr); err != nil {
		t.Fatalf("writing program header: %v", err)
	}
	buf.Write(note.Bytes())

	if err := os.WriteFile(path, buf.Bytes(), 0644); err != nil {
		t.Fatalf("writing synthetic ELF %s: %v", path, err)
	}
}

const elflibNTGnuBuildID = 3

func assertSameInode(t *testing.T, a, b string) {
	t.Helper()
	fiA, err := os.Stat(a)
	if err != nil {
		t.Fatalf("stat(%s): %v", a, err)
	}
	fiB, err := os.Stat(b)
	if err != nil {
		t.Fatalf("stat(%s): %v", b, err)
	}
	if !os.SameFile(fiA, fiB) {
		t.Fatalf("expected %s and %s to be hardlinks to the same file", a, b)
	}
}

func TestRunNormalStrippedAndDebug(t *testing.T) {
	tmp := t.TempDir()
	buildIDDir := filepath.Join(tmp, ".build-id")
	stampPath := filepath.Join(tmp, "bin.build-id.stamp")
	stripped := filepath.Join(tmp, "bin")
	unstripped := filepath.Join(tmp, "bin.unstripped")

	rawID := []byte{0xab, 0xcd, 0xef, 0x01, 0x23, 0x45, 0x67, 0x89}
	wantID := hex.EncodeToString(rawID)
	writeSyntheticELF(t, stripped, rawID)
	writeSyntheticELF(t, unstripped, rawID)

	entries := entryList{
		{suffix: "", file: stripped},
		{suffix: ".debug", file: unstripped},
	}
	if err := run(buildIDDir, stampPath, entries, func(string) bool { return false }); err != nil {
		t.Fatalf("run() unexpected error: %v", err)
	}

	gotStamp, err := os.ReadFile(stampPath)
	if err != nil {
		t.Fatalf("reading stamp: %v", err)
	}
	if string(gotStamp) != wantID {
		t.Fatalf("stamp = %q, want %q", string(gotStamp), wantID)
	}
	assertSameInode(t, stripped, filepath.Join(buildIDDir, wantID[:2], wantID[2:]))
	assertSameInode(t, unstripped, filepath.Join(buildIDDir, wantID[:2], wantID[2:]+".debug"))
}

func TestRunMismatchedBuildIDsFail(t *testing.T) {
	tmp := t.TempDir()
	buildIDDir := filepath.Join(tmp, ".build-id")
	stampPath := filepath.Join(tmp, "bin.build-id.stamp")
	stripped := filepath.Join(tmp, "bin")
	unstripped := filepath.Join(tmp, "bin.unstripped")

	writeSyntheticELF(t, stripped, []byte{0x11, 0x22, 0x33, 0x44})
	writeSyntheticELF(t, unstripped, []byte{0x55, 0x66, 0x77, 0x88})

	entries := entryList{
		{suffix: "", file: stripped},
		{suffix: ".debug", file: unstripped},
	}
	err := run(buildIDDir, stampPath, entries, func(string) bool { return false })
	if err == nil || !strings.Contains(err.Error(), "do not have the same build ID") {
		t.Fatalf("run() error = %v, want mismatch error", err)
	}
	if _, statErr := os.Stat(stampPath); !os.IsNotExist(statErr) {
		t.Fatalf("expected stamp file not to be written on error")
	}
}

func TestRunVirtualizedDebugStubSkipsOpen(t *testing.T) {
	for _, tc := range []struct {
		name       string
		debugFirst bool
	}{
		{name: "stripped_first", debugFirst: false},
		{name: "debug_first", debugFirst: true},
	} {
		t.Run(tc.name, func(t *testing.T) {
			tmp := t.TempDir()
			buildIDDir := filepath.Join(tmp, ".build-id")
			stampPath := filepath.Join(tmp, "bin.build-id.stamp")
			stripped := filepath.Join(tmp, "bin")
			unstripped := filepath.Join(tmp, "bin.unstripped")

			rawID := []byte{0xde, 0xad, 0xbe, 0xef, 0x10, 0x20, 0x30, 0x40}
			wantID := hex.EncodeToString(rawID)
			writeSyntheticELF(t, stripped, rawID)
			// Write corrupt ELF header AND revoke read permission on the stub:
			// if buildidtool calls os.Open() on it, run() will fail with EACCES
			// or ELF parse error. os.Link() succeeds without opening the file.
			if err := os.WriteFile(unstripped, []byte("\x7fELFnot-a-valid-elf"), 0000); err != nil {
				t.Fatalf("creating stub: %v", err)
			}
			t.Cleanup(func() { _ = os.Chmod(unstripped, 0644) })

			var entries entryList
			if tc.debugFirst {
				entries = entryList{
					{suffix: ".debug", file: unstripped},
					{suffix: "", file: stripped},
				}
			} else {
				entries = entryList{
					{suffix: "", file: stripped},
					{suffix: ".debug", file: unstripped},
				}
			}

			isStub := func(path string) bool { return path == unstripped }
			if err := run(buildIDDir, stampPath, entries, isStub); err != nil {
				t.Fatalf("run() unexpectedly opened or failed on virtualized .debug stub: %v", err)
			}

			gotStamp, err := os.ReadFile(stampPath)
			if err != nil {
				t.Fatalf("reading stamp: %v", err)
			}
			if string(gotStamp) != wantID {
				t.Fatalf("stamp = %q, want %q", string(gotStamp), wantID)
			}
			assertSameInode(t, stripped, filepath.Join(buildIDDir, wantID[:2], wantID[2:]))
			assertSameInode(t, unstripped, filepath.Join(buildIDDir, wantID[:2], wantID[2:]+".debug"))
		})
	}
}

func TestRunVirtualizedDebugOnlyStillOpens(t *testing.T) {
	tmp := t.TempDir()
	buildIDDir := filepath.Join(tmp, ".build-id")
	stampPath := filepath.Join(tmp, "bin.build-id.stamp")
	unstripped := filepath.Join(tmp, "bin.unstripped")

	rawID := []byte{0xca, 0xfe, 0xba, 0xbe, 0x01, 0x02}
	wantID := hex.EncodeToString(rawID)
	writeSyntheticELF(t, unstripped, rawID)

	entries := entryList{
		{suffix: ".debug", file: unstripped},
	}
	if err := run(buildIDDir, stampPath, entries, func(string) bool { return true }); err != nil {
		t.Fatalf("run() unexpected error: %v", err)
	}

	gotStamp, err := os.ReadFile(stampPath)
	if err != nil {
		t.Fatalf("reading stamp: %v", err)
	}
	if string(gotStamp) != wantID {
		t.Fatalf("stamp = %q, want %q", string(gotStamp), wantID)
	}
	assertSameInode(t, unstripped, filepath.Join(buildIDDir, wantID[:2], wantID[2:]+".debug"))
}

func TestRunStrippedWithoutBuildIDFallsBackToVirtualizedDebug(t *testing.T) {
	tmp := t.TempDir()
	buildIDDir := filepath.Join(tmp, ".build-id")
	stampPath := filepath.Join(tmp, "bin.build-id.stamp")
	stripped := filepath.Join(tmp, "bin")
	unstripped := filepath.Join(tmp, "bin.unstripped")

	if err := os.WriteFile(stripped, []byte("not-an-elf"), 0644); err != nil {
		t.Fatalf("writing non-ELF: %v", err)
	}
	rawID := []byte{0x42, 0x43, 0x44, 0x45}
	wantID := hex.EncodeToString(rawID)
	writeSyntheticELF(t, unstripped, rawID)

	entries := entryList{
		{suffix: "", file: stripped},
		{suffix: ".debug", file: unstripped},
	}
	isStub := func(path string) bool { return path == unstripped }
	if err := run(buildIDDir, stampPath, entries, isStub); err != nil {
		t.Fatalf("run() unexpected error: %v", err)
	}

	gotStamp, err := os.ReadFile(stampPath)
	if err != nil {
		t.Fatalf("reading stamp: %v", err)
	}
	if string(gotStamp) != wantID {
		t.Fatalf("stamp = %q, want %q", string(gotStamp), wantID)
	}
	assertSameInode(t, unstripped, filepath.Join(buildIDDir, wantID[:2], wantID[2:]+".debug"))
	if _, err := os.Stat(filepath.Join(buildIDDir, wantID[:2], wantID[2:])); !os.IsNotExist(err) {
		t.Fatalf("expected stripped entry without build ID not to be linked")
	}
}

func TestRunSameFileTwice(t *testing.T) {
	tmp := t.TempDir()
	buildIDDir := filepath.Join(tmp, ".build-id")
	stampPath := filepath.Join(tmp, "bin.build-id.stamp")
	bin := filepath.Join(tmp, "bin")

	rawID := []byte{0x99, 0x88, 0x77, 0x66}
	wantID := hex.EncodeToString(rawID)
	writeSyntheticELF(t, bin, rawID)

	entries := entryList{
		{suffix: ".debug", file: bin},
		{suffix: "", file: bin},
	}
	if err := run(buildIDDir, stampPath, entries, func(string) bool { return false }); err != nil {
		t.Fatalf("run() unexpected error: %v", err)
	}

	assertSameInode(t, bin, filepath.Join(buildIDDir, wantID[:2], wantID[2:]))
	assertSameInode(t, bin, filepath.Join(buildIDDir, wantID[:2], wantID[2:]+".debug"))
}

func TestRunRemovesOldLinksOnRebuild(t *testing.T) {
	tmp := t.TempDir()
	buildIDDir := filepath.Join(tmp, ".build-id")
	stampPath := filepath.Join(tmp, "bin.build-id.stamp")
	stripped := filepath.Join(tmp, "bin")
	unstripped := filepath.Join(tmp, "bin.unstripped")

	oldID := []byte{0xaa, 0xbb, 0xcc, 0xdd}
	oldHex := hex.EncodeToString(oldID)
	writeSyntheticELF(t, stripped, oldID)
	writeSyntheticELF(t, unstripped, oldID)

	entries := entryList{
		{suffix: "", file: stripped},
		{suffix: ".debug", file: unstripped},
	}
	isStub := func(path string) bool { return path == unstripped }
	if err := run(buildIDDir, stampPath, entries, isStub); err != nil {
		t.Fatalf("first run() error: %v", err)
	}

	newID := []byte{0x12, 0x34, 0x56, 0x78}
	newHex := hex.EncodeToString(newID)
	writeSyntheticELF(t, stripped, newID)
	writeSyntheticELF(t, unstripped, newID)
	if err := run(buildIDDir, stampPath, entries, isStub); err != nil {
		t.Fatalf("second run() error: %v", err)
	}

	for _, oldPath := range []string{
		filepath.Join(buildIDDir, oldHex[:2], oldHex[2:]),
		filepath.Join(buildIDDir, oldHex[:2], oldHex[2:]+".debug"),
	} {
		if _, err := os.Stat(oldPath); !os.IsNotExist(err) {
			t.Fatalf("expected old link %s to be removed", oldPath)
		}
	}
	assertSameInode(t, stripped, filepath.Join(buildIDDir, newHex[:2], newHex[2:]))
	assertSameInode(t, unstripped, filepath.Join(buildIDDir, newHex[:2], newHex[2:]+".debug"))
}

func TestRunNoBuildIDWritesEmptyStamp(t *testing.T) {
	tmp := t.TempDir()
	buildIDDir := filepath.Join(tmp, ".build-id")
	stampPath := filepath.Join(tmp, "bin.build-id.stamp")
	nonELF := filepath.Join(tmp, "script")
	if err := os.WriteFile(nonELF, []byte("#!/bin/sh\n"), 0755); err != nil {
		t.Fatalf("writing non-ELF: %v", err)
	}

	entries := entryList{{suffix: "", file: nonELF}}
	if err := run(buildIDDir, stampPath, entries, func(string) bool { return false }); err != nil {
		t.Fatalf("run() unexpected error: %v", err)
	}
	gotStamp, err := os.ReadFile(stampPath)
	if err != nil {
		t.Fatalf("reading stamp: %v", err)
	}
	if len(gotStamp) != 0 {
		t.Fatalf("stamp = %q, want empty", string(gotStamp))
	}
}
