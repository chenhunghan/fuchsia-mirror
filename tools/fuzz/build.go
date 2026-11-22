// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package fuzz

import (
	"bufio"
	"encoding/json"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"regexp"
	"runtime"
	"strconv"
	"strings"

	"github.com/golang/glog"
	"go.fuchsia.dev/fuchsia/tools/lib/productbundle"
)

// A Build represents a Fuchsia build, consisting of all the resources needed
// to run a fuzzer on an instance (e.g. a Fuchsia image, fuzzer packages and
// metadata, binary symbols, support utilities, etc.).
type Build interface {
	// Ensures all the needed resources are present and fetches any that are
	// missing. Multiple calls to Prepare should be idempotent for the same
	// Build.
	Prepare() error

	// Returns a fuzzer specified by a `package/binary` name, or an error if it
	// isn't found.
	Fuzzer(name string) (*Fuzzer, error)

	// Returns the absolute host paths for each key.  Each key corresponds to a
	// specific resource provided by the Build.  This abstraction allows for
	// different build types to have different structures.
	Path(keys ...string) ([]string, error)

	// Reads input from `in`, symbolizes it, and writes it back to `out`.
	// Returns on error, or when `in` has no more data to read.  Processing
	// will be streamed, line-by-line.
	// TODO(https://fxbug.dev/42124233): does this belong elsewhere?
	Symbolize(in io.ReadCloser, out io.Writer) error

	// Returns a list of the names of the fuzzers that are available to run
	ListFuzzers() []string
}

// BaseBuild is a simple implementation of the Build interface
type BaseBuild struct {
	Fuzzers map[string]*Fuzzer
	Paths   map[string]string
	IDs     []string
}

// This is stubbed out to allow for test code to replace it
var NewBuild = NewBuildFromEnvironment

// This environment variable is set by the ClusterFuzz build manager
const clusterFuzzBundleDirEnvVar = "FUCHSIA_RESOURCES_DIR"

// Attempt to auto-detect the correct Build type
func NewBuildFromEnvironment() (Build, error) {
	if _, found := os.LookupEnv(clusterFuzzBundleDirEnvVar); found {
		return NewClusterFuzzLegacyBuild()
	}
	return NewLocalFuchsiaBuild()
}

// NewClusterFuzzLegacyBuild will create a BaseBuild with path layouts
// corresponding to the legacy build bundles used by ClusterFuzz's original
// Python integration. Note that these build bundles only support x64.
func NewClusterFuzzLegacyBuild() (Build, error) {
	bundleDir, found := os.LookupEnv(clusterFuzzBundleDirEnvVar)
	if !found {
		return nil, fmt.Errorf("%s not set", clusterFuzzBundleDirEnvVar)
	}
	absBundleDir, err := filepath.Abs(bundleDir)
	if err != nil {
		return nil, fmt.Errorf("failed to resolve absolute path for %q: %s", bundleDir, err)
	}
	bundleDir = absBundleDir

	buildDir := filepath.Join(bundleDir, "build")
	targetDir := filepath.Join(bundleDir, "target", "x64")
	clangDir := filepath.Join(buildDir, "buildtools", "linux-x64", "clang")

	blkPath := filepath.Join(targetDir, "fvm.blk")
	for _, candidate := range []string{"fxfs.sparse.blk", "fxfs.blk", "fvm.blk"} {
		if p := filepath.Join(targetDir, candidate); fileExists(p) {
			blkPath = p
			break
		}
	}

	kernelPath := filepath.Join(targetDir, "multiboot.bin")
	for _, candidate := range []string{"linux-x86-boot-shim.bin", "qemu-boot-shim.bin", "multiboot.bin"} {
		if p := filepath.Join(targetDir, candidate); fileExists(p) {
			kernelPath = p
			break
		}
	}

	symbolizerDir := filepath.Join(buildDir, "zircon", "prebuilt", "downloads", "symbolize", "linux-x64")
	symbolizerPath := filepath.Join(symbolizerDir, "symbolizer")

	build := &BaseBuild{
		Paths: map[string]string{
			"zbi":        filepath.Join(targetDir, "fuchsia.zbi"),
			"fvm":        filepath.Join(buildDir, "out", "default.zircon", "tools", "fvm"),
			"simg2img":   filepath.Join(bundleDir, "simg2img"),
			"zbitool":    filepath.Join(buildDir, "out", "default.zircon", "tools", "zbi"),
			"blk":        blkPath,
			"qemu":       filepath.Join(bundleDir, "qemu-for-fuchsia", "bin", "qemu-system-x86_64"),
			"kernel":     kernelPath,
			"symbolizer": symbolizerPath,
			"tests.json": filepath.Join(buildDir, "out", "default", "tests.json"),
			"ffx":        filepath.Join(bundleDir, "ffx"),
		},
		IDs: []string{
			filepath.Join(clangDir, "lib", "debug", ".build_id"),
			filepath.Join(buildDir, "out", "default", ".build-id"),
			filepath.Join(buildDir, "out", "default.zircon", ".build-id"),
		},
	}
	if err := build.LoadFuzzers(); err != nil {
		return nil, err
	}

	return build, nil
}

var Platforms = map[string]string{
	"linux":  "linux",
	"darwin": "mac",
}

var Archs = map[string]struct {
	Binary string
	Kernel string
}{
	"x64":   {"qemu-system-x86_64", "linux-x86-boot-shim.bin"},
	"arm64": {"qemu-system-aarch64", "qemu-boot-shim.bin"},
}

var hostDir = map[string]string{"arm64": "host_arm64", "amd64": "host_x64"}[runtime.GOARCH]

type productBundleMetadata struct {
	Name string `json:"name"`
	Path string `json:"path"`
}

func resolveProductBundleImages(buildDir, productName, boardName string) (zbiPath, blkPath, kernelPath string, err error) {
	pbManifestPath := filepath.Join(buildDir, "product_bundles.json")
	data, err := os.ReadFile(pbManifestPath)
	if err != nil {
		return "", "", "", fmt.Errorf("failed to read %q: %s", pbManifestPath, err)
	}
	var entries []productBundleMetadata
	if err := json.Unmarshal(data, &entries); err != nil {
		return "", "", "", fmt.Errorf("failed to parse %q: %s", pbManifestPath, err)
	}
	if len(entries) == 0 {
		return "", "", "", fmt.Errorf("no product bundles in %q", pbManifestPath)
	}

	selected := entries[0]
	if productName != "" && boardName != "" {
		wantName := productName + "." + boardName
		found := false
		for _, entry := range entries {
			if entry.Name == wantName {
				selected = entry
				found = true
				break
			}
		}
		if !found {
			return "", "", "", fmt.Errorf("product bundle %q not found in %q", wantName, pbManifestPath)
		}
	}

	pbDir := selected.Path
	if !filepath.IsAbs(pbDir) {
		pbDir = filepath.Join(buildDir, pbDir)
	}
	pb, err := productbundle.Load(pbDir)
	if err != nil {
		return "", "", "", err
	}

	zbiPath, err = pb.GetSystemAImage("zbi", "zircon-a")
	if err != nil {
		return "", "", "", err
	}
	kernelPath, err = pb.GetSystemAImage("kernel", "qemu-kernel")
	if err != nil {
		return "", "", "", err
	}
	blkPath, err = pb.GetSystemAImage("blk", "fxfs.fastboot")
	if err != nil {
		blkPath, err = pb.GetSystemAImage("blk", "storage-full")
	}
	if err != nil {
		return "", "", "", fmt.Errorf("failed to find system_a blk image (fxfs.fastboot or storage-full) in %q", pbDir)
	}
	return zbiPath, blkPath, kernelPath, nil
}

// NewLocalFuchsiaBuild will create a BaseBuild with path layouts corresponding
// to a local Fuchsia checkout
func NewLocalFuchsiaBuild() (Build, error) {
	fuchsiaDir := os.Getenv("FUCHSIA_DIR")
	if fuchsiaDir == "" {
		// Fall back to relative path from this file
		fuchsiaDir = filepath.Join("..", "..")
	}
	absFuchsiaDir, err := filepath.Abs(fuchsiaDir)
	if err != nil {
		return nil, fmt.Errorf("failed to resolve absolute path for %q: %s", fuchsiaDir, err)
	}
	fuchsiaDir = absFuchsiaDir

	fxBuildDir := filepath.Join(fuchsiaDir, ".fx-build-dir")
	contents, err := os.ReadFile(fxBuildDir)
	if err != nil {
		return nil, fmt.Errorf("failed to read %q: %s", fxBuildDir, err)
	}

	buildDir := strings.TrimSpace(string(contents))
	if !filepath.IsAbs(buildDir) {
		buildDir = filepath.Join(fuchsiaDir, buildDir)
	}
	prebuiltDir := filepath.Join(fuchsiaDir, "prebuilt")

	platform, ok := Platforms[runtime.GOOS]
	if !ok {
		return nil, fmt.Errorf("unsupported os: %s", runtime.GOOS)
	}

	ffxPath, err := findToolPath(buildDir, "ffx")
	if err != nil {
		return nil, fmt.Errorf("unable to locate ffx: %s", err)
	}

	fxConfig := filepath.Join(buildDir, "fx.config")
	file, err := os.Open(fxConfig)
	if err != nil {
		return nil, fmt.Errorf("failed to open %q: %s", fxConfig, err)
	}
	defer file.Close()

	properties := map[string]string{}
	// Matches fx.config lines of the form KEY='VALUE', KEY=VALUE, or KEY=:
	// m[1] is the variable name, m[2] is the single-quoted value, and m[3]
	// is the unquoted value.
	re := regexp.MustCompile(`^([^=]+)=(?:'([^']+)'|(.+))?$`)
	scanner := bufio.NewScanner(file)
	for scanner.Scan() {
		m := re.FindStringSubmatch(scanner.Text())
		if m != nil {
			val := m[2]
			if val == "" {
				val = m[3]
			}
			properties[m[1]] = val
		}
	}

	arch, found := properties["FUCHSIA_ARCH"]
	if !found {
		return nil, fmt.Errorf("no arch in %s", fxConfig)
	}

	archInfo, ok := Archs[arch]
	if !ok {
		supported := make([]string, 0, len(Archs))
		for k := range Archs {
			supported = append(supported, k)
		}
		return nil, fmt.Errorf("unsupported arch: %s (supported: %v)", arch, supported)
	}

	binary := archInfo.Binary
	kernel := archInfo.Kernel
	platform += "-" + arch

	clangDir := filepath.Join(prebuiltDir, "third_party/clang", platform)
	qemuDir := filepath.Join(prebuiltDir, "third_party/qemu", platform)
	imgDir := filepath.Join(buildDir, "obj", "build", "images", "fuchsia", "fuchsia")

	zbiPath := filepath.Join(imgDir, "fuchsia.zbi")
	blkPath := filepath.Join(imgDir, "fvm.blk")
	kernelPath := filepath.Join(buildDir, kernel)

	if fileExists(filepath.Join(buildDir, "product_bundles.json")) {
		pbZbi, pbBlk, pbKernel, err := resolveProductBundleImages(
			buildDir, properties["FUCHSIA_PRODUCT_NAME"], properties["FUCHSIA_BOARD_NAME"])
		if err != nil {
			return nil, fmt.Errorf("failed to resolve product bundle images: %s", err)
		}
		zbiPath = pbZbi
		blkPath = pbBlk
		kernelPath = pbKernel
	}

	build := &BaseBuild{
		Paths: map[string]string{
			"zbi":        zbiPath,
			"fvm":        filepath.Join(buildDir, hostDir, "fvm"),
			"simg2img":   filepath.Join(prebuiltDir, "third_party/libsparse/bin/simg2img"),
			"zbitool":    filepath.Join(buildDir, hostDir, "zbi"),
			"blk":        blkPath,
			"qemu":       filepath.Join(qemuDir, "bin", binary),
			"kernel":     kernelPath,
			"symbolizer": filepath.Join(buildDir, hostDir, "symbolizer"),
			"tests.json": filepath.Join(buildDir, "tests.json"),
			"ffx":        ffxPath,
		},
		IDs: []string{
			filepath.Join(clangDir, "lib", "debug", ".build-id"),
			filepath.Join(buildDir, ".build-id"),
		},
	}
	if err := build.LoadFuzzers(); err != nil {
		return nil, err
	}

	return build, nil
}

// Subset of data format used in tool_paths.json
type toolMetadata struct {
	Name string
	Path string
}

// Resolve a tool by name using tool_paths.json in the buildDir. This is only
// used when working in a local Fuchsia checkout.
func findToolPath(buildDir string, toolName string) (string, error) {
	jsonPath := filepath.Join(buildDir, "tool_paths.json")
	jsonBlob, err := os.ReadFile(jsonPath)
	if err != nil {
		return "", fmt.Errorf("failed to read %q: %s", jsonPath, err)
	}
	var metadataList []toolMetadata
	if err := json.Unmarshal(jsonBlob, &metadataList); err != nil {
		return "", fmt.Errorf("failed to parse %q: %s", jsonPath, err)
	}

	for _, metadata := range metadataList {
		if metadata.Name == toolName {
			return filepath.Join(buildDir, metadata.Path), nil
		}
	}

	return "", fmt.Errorf("no path found for tool %q in %q", toolName, jsonPath)
}

// LoadFuzzers populates the build's map of Fuzzers. Unless an error is
// returned, any previously loaded fuzzers will be discarded.
func (b *BaseBuild) LoadFuzzers() error {
	v2Fuzzers, err := b.loadV2Fuzzers()
	if err != nil {
		return fmt.Errorf("error loading v2 fuzzers: %s", err)
	}

	b.Fuzzers = make(map[string]*Fuzzer)
	for name, fuzzer := range v2Fuzzers {
		b.Fuzzers[name] = fuzzer
	}

	return nil
}

// Convenience type alias for heterogenous metadata objects
type fuzzerMetadata map[string]string

// Relevant subset of tests.json metadata format
type testMetadata struct {
	BuildRule  string `json:"build_rule"`
	PackageUrl string `json:"package_url"`
}
type testEntry struct {
	Test testMetadata
}

// loadV2Fuzzers reads and parses tests.json.
func (b *BaseBuild) loadV2Fuzzers() (map[string]*Fuzzer, error) {
	ffxFuzzVal, ok := os.LookupEnv("UNDERCOAT_USE_FFX_FUZZ")
	if !ok {
		ffxFuzzVal = "0"
	}
	useFfxFuzz, err := strconv.ParseBool(ffxFuzzVal)
	if err != nil {
		return nil, fmt.Errorf("failed to parse UNDERCOAT_USE_FFX_FUZZ=%s: %s", ffxFuzzVal, err)
	}

	paths, err := b.Path("tests.json")
	if err != nil {
		return nil, err
	}

	jsonPath := paths[0]

	fuzzers := make(map[string]*Fuzzer)

	glog.Infof("Loading fuzzers from %q", jsonPath)

	jsonBlob, err := os.ReadFile(jsonPath)
	if err != nil {
		return nil, fmt.Errorf("failed to read %q: %s", jsonPath, err)
	}

	var testList []testEntry
	if err := json.Unmarshal(jsonBlob, &testList); err != nil {
		return nil, fmt.Errorf("failed to parse %q: %s", jsonPath, err)
	}

	pkgUrlRegex := regexp.MustCompile(`^fuchsia-pkg://[^/]+/([^#]+)#meta/([^\.]+)\.cm$`)
	for _, entry := range testList {
		if entry.Test.BuildRule != "fuchsia_fuzzer_package" {
			continue
		}
		m := pkgUrlRegex.FindStringSubmatch(entry.Test.PackageUrl)
		if m == nil {
			return nil, fmt.Errorf("found test with unexpected url: %v", entry)
		}
		fuzzer := NewV2Fuzzer(b, m[1], m[2], useFfxFuzz)
		fuzzers[fuzzer.Name] = fuzzer
	}

	return fuzzers, nil
}

// ListFuzzers lists the names of fuzzers present in the build, excluding any
// that we don't want ClusterFuzz to actually pick up. We can't just omit the
// example fuzzers from the build entirely because they are used in integration
// testing.
func (b *BaseBuild) ListFuzzers() []string {
	var names []string
	for _, fuzzer := range b.Fuzzers {
		if !fuzzer.IsExample() {
			names = append(names, fuzzer.Name)
		}
	}
	return names
}

// Fuzzer finds the Fuzzer with the given name, if available
func (b *BaseBuild) Fuzzer(name string) (*Fuzzer, error) {
	fuzzer, found := b.Fuzzers[name]
	if !found {
		return nil, fmt.Errorf("no such fuzzer: %s", name)
	}
	return fuzzer, nil
}

// Prepare is a no-op for simple builds
func (b *BaseBuild) Prepare() error {
	return nil
}

// Path returns the absolute paths to the list of files indicated by keys. This
// allows callers to abstract away the detail of where specific file resources
// are.
func (b *BaseBuild) Path(keys ...string) ([]string, error) {
	paths := make([]string, len(keys))
	for i, key := range keys {
		if path, found := b.Paths[key]; found {
			paths[i] = path
		} else {
			return nil, fmt.Errorf("no path for %q", key)
		}
	}
	return paths, nil
}

var logPrefixRegex = regexp.MustCompile(`[0-9\[\]\.]*\[klog\] (?:INFO|WARNING|ERROR): `)

// Remove timestamps, etc.
func stripLogPrefix(line string) string {
	return logPrefixRegex.ReplaceAllString(line, "")
}

// Symbolize reads from in and replaces symbolizer markup with debug
// information before writing the result to out.  This is blocking, and does
// not propagate EOFs from in to out.
func (b *BaseBuild) Symbolize(in io.ReadCloser, out io.Writer) error {
	// Close `in` on return so that the fuzzer doesn't block on a write if an
	// early exit occurs later in the output-processing chain.
	defer in.Close()

	paths, err := b.Path("symbolizer")
	if err != nil {
		return err
	}

	args := make([]string, 0, 2*len(b.IDs))
	for _, dir := range b.IDs {
		args = append(args, "--build-id-dir", dir)
	}
	cmd := NewCommand(paths[0], args...)
	cmd.Stdin = in
	pipe, err := cmd.StdoutPipe()
	if err != nil {
		return err
	}
	if err := cmd.Start(); err != nil {
		return err
	}
	scanner := bufio.NewScanner(pipe)

	for scanner.Scan() {
		io.WriteString(out, stripLogPrefix(scanner.Text())+"\n")
	}

	if err := scanner.Err(); err != nil {
		return fmt.Errorf("failed during scan: %s", err)
	}

	return cmd.Wait()
}
