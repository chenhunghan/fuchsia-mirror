// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package artifactory

import (
	"bytes"
	"debug/elf"
	"encoding/binary"
	"encoding/hex"
	"os"
	"path/filepath"
	"testing"

	"github.com/google/go-cmp/cmp"
	"github.com/google/go-cmp/cmp/cmpopts"

	"go.fuchsia.dev/fuchsia/tools/debug/elflib"
	"go.fuchsia.dev/fuchsia/tools/lib/jsonutil"
)

// writeTestELFWithBuildID writes a minimal 64-bit little-endian ELF file at
// path containing a single PT_NOTE program header with a GNU build ID note
// (.note.gnu.build-id) matching buildIDHex.
//
// This is needed because debugSymbolUploads verifies that each ELF file on disk
// contains a build ID note matching its debug_symbols.json entry via
// elflib.BinaryFileRef.Verify(), which parses the ELF header and PT_NOTE
// segments.
func writeTestELFWithBuildID(t *testing.T, path, buildIDHex string) {
	t.Helper()
	buildID, err := hex.DecodeString(buildIDHex)
	if err != nil {
		t.Fatalf("hex.DecodeString(%q) failed: %v", buildIDHex, err)
	}

	// Construct the ELF note payload: a 12-byte note header (namesz, descsz,
	// type=NT_GNU_BUILD_ID), followed by the "GNU\x00" owner name and raw build
	// ID bytes, padded to 4-byte alignment.
	var noteBuf bytes.Buffer
	const gnuNoteName = "GNU\x00"
	noteHdr := struct {
		Namesz uint32
		Descsz uint32
		Type   uint32
	}{
		Namesz: uint32(len(gnuNoteName)),
		Descsz: uint32(len(buildID)),
		Type:   elflib.NT_GNU_BUILD_ID,
	}
	if err := binary.Write(&noteBuf, binary.LittleEndian, &noteHdr); err != nil {
		t.Fatalf("binary.Write(noteHdr) failed: %v", err)
	}
	if _, err := noteBuf.WriteString(gnuNoteName); err != nil {
		t.Fatalf("noteBuf.WriteString(%q) failed: %v", gnuNoteName, err)
	}
	if _, err := noteBuf.Write(buildID); err != nil {
		t.Fatalf("noteBuf.Write(buildID) failed: %v", err)
	}
	for noteBuf.Len()%4 != 0 {
		if err := noteBuf.WriteByte(0); err != nil {
			t.Fatalf("noteBuf.WriteByte(0) failed: %v", err)
		}
	}
	noteBytes := noteBuf.Bytes()

	// Construct a minimal 64-bit ELF header followed immediately by a single
	// PT_NOTE program header pointing to the note payload above.
	hdr := elf.Header64{
		Type:      uint16(elf.ET_EXEC),
		Machine:   uint16(elf.EM_X86_64),
		Version:   uint32(elf.EV_CURRENT),
		Phoff:     uint64(binary.Size(elf.Header64{})),
		Ehsize:    uint16(binary.Size(elf.Header64{})),
		Phentsize: uint16(binary.Size(elf.Prog64{})),
		Phnum:     1,
	}
	copy(hdr.Ident[:], elf.ELFMAG)
	hdr.Ident[elf.EI_CLASS] = byte(elf.ELFCLASS64)
	hdr.Ident[elf.EI_DATA] = byte(elf.ELFDATA2LSB)
	hdr.Ident[elf.EI_VERSION] = byte(elf.EV_CURRENT)

	prog := elf.Prog64{
		Type:   uint32(elf.PT_NOTE),
		Off:    uint64(binary.Size(elf.Header64{}) + binary.Size(elf.Prog64{})),
		Filesz: uint64(len(noteBytes)),
		Memsz:  uint64(len(noteBytes)),
		Align:  4,
	}

	var elfBuf bytes.Buffer
	if err := binary.Write(&elfBuf, binary.LittleEndian, &hdr); err != nil {
		t.Fatalf("binary.Write(hdr) failed: %v", err)
	}
	if err := binary.Write(&elfBuf, binary.LittleEndian, &prog); err != nil {
		t.Fatalf("binary.Write(prog) failed: %v", err)
	}
	if _, err := elfBuf.Write(noteBytes); err != nil {
		t.Fatalf("elfBuf.Write(noteBytes) failed: %v", err)
	}

	if err := os.MkdirAll(filepath.Dir(path), 0o700); err != nil {
		t.Fatalf("os.MkdirAll(%q) failed: %v", filepath.Dir(path), err)
	}
	if err := os.WriteFile(path, elfBuf.Bytes(), 0o600); err != nil {
		t.Fatalf("os.WriteFile(%q) failed: %v", path, err)
	}
}

func TestDebugSymbolUploads(t *testing.T) {
	checkout := t.TempDir()
	outputDir := filepath.Join(checkout, "out")
	if err := os.Mkdir(outputDir, 0o700); err != nil {
		t.Fatal(err)
	}

	debugSymbols := []ExportedDebugSymbol{
		{
			Debug:    filepath.Join(".build-id", "pr", "ebuiltA.debug"),
			Breakpad: filepath.Join(".build-id", "pr", "ebuiltA.sym"),
			GSYM:     filepath.Join(".build-id", "pr", "ebuiltA.gsym"),
			BuildID:  "b0000001",
			OS:       "fuchsia",
			CPU:      "arm64",
			Label:    "//prebuilt",
		},
		{
			Debug:    filepath.Join(".build-id", "pr", "ebuiltB.debug"),
			Breakpad: filepath.Join(".build-id", "pr", "ebuiltB.sym"),
			BuildID:  "b0000002",
			OS:       "linux",
			CPU:      "arm64",
			Label:    "//prebuilt",
		},
		{
			Debug:    filepath.Join(".build-id", "fi", "rst.debug"),
			DestPath: filepath.Join(".build-id", "fi", "rst"),
			Breakpad: filepath.Join(".build-id", "fi", "rst.sym"),
			GSYM:     filepath.Join(".build-id", "fi", "rst.gsym"),
			BuildID:  "b0000003",
			OS:       "fuchsia",
			CPU:      "arm64",
			Label:    "//first",
		},
		{
			Debug:    filepath.Join(".build-id", "se", "cond.debug"),
			Breakpad: filepath.Join(".build-id", "se", "cond.sym"),
			BuildID:  "b0000004",
			OS:       "linux",
			CPU:      "x64",
			Label:    "//second",
		},
		{
			Debug:    filepath.Join(".build-id", "th", "ird.debug"),
			Stripped: filepath.Join(".build-id", "th", "ird"),
			DestPath: filepath.Join(".build-id", "th", "ird"),
			BuildID:  "b0000005",
			OS:       "linux",
			CPU:      "x64",
			Label:    "//third",
		},
	}

	// Write valid debug symbols to be verified before upload.
	for _, entry := range debugSymbols {
		writeTestELFWithBuildID(t, filepath.Join(outputDir, entry.Debug), entry.BuildID)
		if entry.Stripped != "" {
			writeTestELFWithBuildID(t, filepath.Join(outputDir, entry.Stripped), entry.BuildID)
		}
	}
	if err := jsonutil.WriteToFile(filepath.Join(outputDir, "debug_symbols.json"), debugSymbols); err != nil {
		t.Fatal(err)
	}

	// Mapping from each file's local filepath to the locations in GCS to which
	// the file should be uploaded.
	expectedUploadDestinations := map[string][]string{
		filepath.Join(outputDir, "build-ids.txt"): {
			"TOP_NAMESPACE/build-ids.txt",
		},
		filepath.Join(outputDir, "build-ids.json"): {
			"TOP_NAMESPACE/build-ids.json",
		},
		filepath.Join(outputDir, "debug_symbols.json"): {
			"TOP_NAMESPACE/debug_symbols.json",
		},
		filepath.Join(outputDir, ".build-id", "fi", "rst.debug"): {
			"DEBUG_NAMESPACE/b0000003.debug",
			"BUILDID_NAMESPACE/b0000003/debuginfo",
			"BUILDID_NAMESPACE/b0000003/executable",
		},
		filepath.Join(outputDir, ".build-id", "fi", "rst.sym"): {
			"DEBUG_NAMESPACE/b0000003.sym",
			"BUILDID_NAMESPACE/b0000003/breakpad",
		},
		filepath.Join(outputDir, ".build-id", "fi", "rst.gsym"): {
			"DEBUG_NAMESPACE/b0000003.gsym",
			"BUILDID_NAMESPACE/b0000003/gsym",
		},
		filepath.Join(outputDir, ".build-id", "pr", "ebuiltA.debug"): {
			"DEBUG_NAMESPACE/b0000001.debug",
			"BUILDID_NAMESPACE/b0000001/debuginfo",
			"BUILDID_NAMESPACE/b0000001/executable",
		},
		filepath.Join(outputDir, ".build-id", "pr", "ebuiltA.sym"): {
			"DEBUG_NAMESPACE/b0000001.sym",
			"BUILDID_NAMESPACE/b0000001/breakpad",
		},
		filepath.Join(outputDir, ".build-id", "pr", "ebuiltA.gsym"): {
			"DEBUG_NAMESPACE/b0000001.gsym",
			"BUILDID_NAMESPACE/b0000001/gsym",
		},
		filepath.Join(outputDir, ".build-id", "pr", "ebuiltB.debug"): {
			"DEBUG_NAMESPACE/b0000002.debug",
			"BUILDID_NAMESPACE/b0000002/debuginfo",
			"BUILDID_NAMESPACE/b0000002/executable",
		},
		filepath.Join(outputDir, ".build-id", "pr", "ebuiltB.sym"): {
			"DEBUG_NAMESPACE/b0000002.sym",
			"BUILDID_NAMESPACE/b0000002/breakpad",
		},
		filepath.Join(outputDir, ".build-id", "se", "cond.debug"): {
			"DEBUG_NAMESPACE/b0000004.debug",
			"BUILDID_NAMESPACE/b0000004/debuginfo",
			"BUILDID_NAMESPACE/b0000004/executable",
		},
		filepath.Join(outputDir, ".build-id", "se", "cond.sym"): {
			"DEBUG_NAMESPACE/b0000004.sym",
			"BUILDID_NAMESPACE/b0000004/breakpad",
		},
		filepath.Join(outputDir, ".build-id", "th", "ird.debug"): {
			"DEBUG_NAMESPACE/b0000005.debug",
			"BUILDID_NAMESPACE/b0000005/debuginfo",
		},
		filepath.Join(outputDir, ".build-id", "th", "ird"): {
			"BUILDID_NAMESPACE/b0000005/executable",
		},
	}

	// All uploads should be compressed, except those in the following set.
	// See https://fxbug.dev/498773554 and https://fxbug.dev/42155140 for details.
	expectedUncompressedUploads := map[string]bool{
		filepath.Join(outputDir, "build-ids.txt"):      true,
		filepath.Join(outputDir, "debug_symbols.json"): true,
	}

	var expectedUploads []Upload
	for src, destinations := range expectedUploadDestinations {
		_, uncompress := expectedUncompressedUploads[src]
		for _, dest := range destinations {
			expectedUploads = append(expectedUploads, Upload{
				Source:      src,
				Destination: dest,
				Compress:    !uncompress,
				Deduplicate: true,
			})
		}
	}

	actualUploads, err := debugSymbolUploads(outputDir, "TOP_NAMESPACE", "DEBUG_NAMESPACE", "BUILDID_NAMESPACE")
	if err != nil {
		t.Fatalf("failed to generate debug binary uploads: %v", err)
	}
	opts := cmpopts.SortSlices(func(a, b Upload) bool { return a.Destination < b.Destination })
	if diff := cmp.Diff(expectedUploads, actualUploads, opts); diff != "" {
		t.Fatalf("unexpected debug binary uploads (-want +got):\n%s", diff)
	}
}

func TestDebugSymbolUploadsBuildIDVerification(t *testing.T) {
	tests := []struct {
		name              string
		debugFileBuildID  string
		strippedBuildID   string
		withStrippedEntry bool
	}{
		{
			name: "missing_debug_binary",
		},
		{
			name:             "mismatched_debug_binary_build_id",
			debugFileBuildID: "deadbeef",
		},
		{
			name:              "missing_stripped_binary",
			debugFileBuildID:  "b0000001",
			withStrippedEntry: true,
		},
		{
			name:              "mismatched_stripped_binary_build_id",
			debugFileBuildID:  "b0000001",
			withStrippedEntry: true,
			strippedBuildID:   "deadbeef",
		},
	}

	for _, tc := range tests {
		t.Run(tc.name, func(t *testing.T) {
			outputDir := t.TempDir()
			entry := ExportedDebugSymbol{
				Debug:   filepath.Join(".build-id", "b0", "000001.debug"),
				BuildID: "b0000001",
				OS:      "linux",
				CPU:     "x64",
				Label:   "//test",
			}
			if tc.withStrippedEntry {
				entry.Stripped = filepath.Join(".build-id", "b0", "000001")
			}
			if tc.debugFileBuildID != "" {
				writeTestELFWithBuildID(t, filepath.Join(outputDir, entry.Debug), tc.debugFileBuildID)
			}
			if tc.strippedBuildID != "" {
				writeTestELFWithBuildID(t, filepath.Join(outputDir, entry.Stripped), tc.strippedBuildID)
			}
			manifestPath := filepath.Join(outputDir, "debug_symbols.json")
			if err := jsonutil.WriteToFile(manifestPath, []ExportedDebugSymbol{entry}); err != nil {
				t.Fatalf("jsonutil.WriteToFile(%q) failed: %v", manifestPath, err)
			}

			uploads, err := debugSymbolUploads(outputDir, "TOP_NAMESPACE", "DEBUG_NAMESPACE", "BUILDID_NAMESPACE")
			if err == nil {
				t.Errorf("debugSymbolUploads(%q, ...) (entry=%+v, debugFileBuildID=%q, strippedBuildID=%q) = %+v, nil; want non-nil error",
					outputDir, entry, tc.debugFileBuildID, tc.strippedBuildID, uploads)
			}
		})
	}
}
