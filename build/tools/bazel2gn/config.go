// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package bazel2gn

import (
	"bytes"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"maps"
	"os"
	"path/filepath"
	"slices"
	"strings"
)

// ConfigFileName is the optional per-repository or per-subtree configuration
// file that extends bazel2gn with custom Bazel-to-GN rule mappings and GN
// imports without modifying bazel2gn itself.
const ConfigFileName = ".bazel2gn.json"

// Config extends bazel2gn with additional GN imports and Bazel-to-GN rule
// conversion definitions loaded from ConfigFileName files.
type Config struct {
	// GNImports lists additional .gni files imported below the bazel2gn sentinel
	// in generated BUILD.gn files covered by this configuration.
	GNImports []string `json:"gn_imports"`

	// Rules maps Bazel rule or macro names to their GN conversion rules.
	Rules map[string]RuleConfig `json:"rules"`
}

// RuleConfig describes how a custom Bazel rule or macro converts to a GN
// template.
type RuleConfig struct {
	// GNTemplate is the GN template name to emit, or noGNEquivalent
	// ("__NO_GN_EQUIVALENT__") to skip targets of this rule.
	GNTemplate string `json:"gn_template"`

	// AttrsToOmit lists Bazel attribute names to drop when converting to GN.
	AttrsToOmit []string `json:"attrs_to_omit,omitempty"`

	// AttrMap renames Bazel attribute names to GN parameter names.
	AttrMap map[string]string `json:"attr_map,omitempty"`

	// AttrTransformers maps Bazel (or converted GN) attribute names to
	// transformer names (for example "deps", "file_paths", "visibility").
	AttrTransformers map[string]string `json:"attr_transformers,omitempty"`

	// AttrAssignmentOps overrides the GN assignment operator (such as "+=" or
	// "-=") for specific Bazel attribute names.
	AttrAssignmentOps map[string]string `json:"attr_assignment_ops,omitempty"`

	// ExtraGNExpressions lists fixed GN assignments always emitted in the
	// converted target.
	ExtraGNExpressions []string `json:"extra_gn_expressions,omitempty"`

	// DefaultGNExpressions lists default GN assignments emitted unless
	// overridden by a `# @bazel2gn:raw_attribute:` annotation.
	DefaultGNExpressions []string `json:"default_gn_expressions,omitempty"`

	// UnwrapPackagedBinaryAttrs lists Bazel attribute names whose
	// `fx_packaged_binary()` references in the same file should be unwrapped to
	// their underlying GN binary (or rewritten to `renamed_binary()`).
	UnwrapPackagedBinaryAttrs []string `json:"unwrap_packaged_binary_attrs,omitempty"`
}

func (c *Config) lookupRule(bazelRule string) (RuleConfig, bool) {
	if c == nil || len(c.Rules) == 0 {
		return RuleConfig{}, false
	}
	rc, ok := c.Rules[bazelRule]
	return rc, ok
}

func (c *Config) isNoGNEquivalent(bazelRule string) bool {
	if bazelRuleToGNTemplate[bazelRule] == noGNEquivalent {
		return true
	}
	rc, ok := c.lookupRule(bazelRule)
	return ok && rc.GNTemplate == noGNEquivalent
}

func (r RuleConfig) omitsAttr(attr string) bool {
	return slices.Contains(r.AttrsToOmit, attr)
}

func cloneRuleConfig(r RuleConfig) RuleConfig {
	out := RuleConfig{
		GNTemplate:                r.GNTemplate,
		AttrsToOmit:               slices.Clone(r.AttrsToOmit),
		ExtraGNExpressions:        slices.Clone(r.ExtraGNExpressions),
		DefaultGNExpressions:      slices.Clone(r.DefaultGNExpressions),
		UnwrapPackagedBinaryAttrs: slices.Clone(r.UnwrapPackagedBinaryAttrs),
	}
	if r.AttrMap != nil {
		out.AttrMap = maps.Clone(r.AttrMap)
	}
	if r.AttrTransformers != nil {
		out.AttrTransformers = maps.Clone(r.AttrTransformers)
	}
	if r.AttrAssignmentOps != nil {
		out.AttrAssignmentOps = maps.Clone(r.AttrAssignmentOps)
	}
	return out
}

func validateConfig(cfg *Config) error {
	for _, imp := range cfg.GNImports {
		if strings.TrimSpace(imp) == "" {
			return errors.New("gn_imports entries must not be empty")
		}
	}
	for ruleName, rc := range cfg.Rules {
		if ruleName == "" {
			return errors.New("rule name in bazel2gn config must not be empty")
		}
		if strings.HasPrefix(ruleName, "$") {
			return fmt.Errorf("rule name %q in bazel2gn config must not start with '$'", ruleName)
		}
		if _, exists := bazelRuleToGNTemplate[ruleName]; exists {
			return fmt.Errorf("rule %q in bazel2gn config conflicts with built-in bazel2gn rule", ruleName)
		}
		if strings.TrimSpace(rc.GNTemplate) == "" {
			return fmt.Errorf("rule %q in bazel2gn config must specify a non-empty gn_template", ruleName)
		}
		for attr, tName := range rc.AttrTransformers {
			if _, ok := transformerAnnotationNames[tName]; !ok {
				return fmt.Errorf("rule %q attr_transformers[%q] references unknown transformer %q", ruleName, attr, tName)
			}
		}
		for attr, op := range rc.AttrAssignmentOps {
			switch op {
			case "=", "+=", "-=":
			default:
				return fmt.Errorf("rule %q attr_assignment_ops[%q] has unsupported operator %q (want '=', '+=', or '-=')", ruleName, attr, op)
			}
		}
	}
	return nil
}

func mergeConfigs(base, overlay *Config) *Config {
	merged := &Config{
		Rules: map[string]RuleConfig{},
	}
	seenImports := map[string]bool{}
	for _, src := range []*Config{base, overlay} {
		if src == nil {
			continue
		}
		for _, imp := range src.GNImports {
			if !seenImports[imp] {
				seenImports[imp] = true
				merged.GNImports = append(merged.GNImports, imp)
			}
		}
		for ruleName, rc := range src.Rules {
			merged.Rules[ruleName] = cloneRuleConfig(rc)
		}
	}
	if len(merged.GNImports) == 0 && len(merged.Rules) == 0 {
		return nil
	}
	return merged
}

// ParseConfig parses and validates JSON configuration bytes for bazel2gn.
func ParseConfig(data []byte) (*Config, error) {
	dec := json.NewDecoder(bytes.NewReader(data))
	dec.DisallowUnknownFields()

	var cfg Config
	if err := dec.Decode(&cfg); err != nil {
		return nil, fmt.Errorf("parsing %s: %w", ConfigFileName, err)
	}
	if err := dec.Decode(&struct{}{}); !errors.Is(err, io.EOF) {
		if err != nil {
			return nil, fmt.Errorf("parsing %s: unexpected trailing JSON data: %w", ConfigFileName, err)
		}
		return nil, fmt.Errorf("parsing %s: unexpected trailing JSON value", ConfigFileName)
	}
	if err := validateConfig(&cfg); err != nil {
		return nil, err
	}
	return &cfg, nil
}

// LoadConfigForDir loads and merges any ConfigFileName files found along the
// directory hierarchy from fuchsiaDir down to filepath.Join(fuchsiaDir, relDir).
// Returns (nil, nil) if no config files exist along that path.
func LoadConfigForDir(fuchsiaDir, relDir string) (*Config, error) {
	if fuchsiaDir == "" {
		return nil, nil
	}
	cleanRel := filepath.Clean(relDir)
	if cleanRel == "." {
		cleanRel = ""
	}
	if filepath.IsAbs(cleanRel) || !filepath.IsLocal(cleanRel) && cleanRel != "" {
		return nil, fmt.Errorf("directory %q is not local to %s", relDir, fuchsiaDir)
	}

	dirsToCheck := []string{fuchsiaDir}
	if cleanRel != "" {
		cur := fuchsiaDir
		for _, part := range strings.Split(filepath.ToSlash(cleanRel), "/") {
			if part == "" || part == "." {
				continue
			}
			cur = filepath.Join(cur, part)
			dirsToCheck = append(dirsToCheck, cur)
		}
	}

	var merged *Config
	for _, dir := range dirsToCheck {
		cfgPath := filepath.Join(dir, ConfigFileName)
		data, err := os.ReadFile(cfgPath)
		if errors.Is(err, os.ErrNotExist) {
			continue
		}
		if err != nil {
			return nil, fmt.Errorf("reading %s: %w", cfgPath, err)
		}
		cfg, err := ParseConfig(data)
		if err != nil {
			return nil, fmt.Errorf("%s: %w", cfgPath, err)
		}
		merged = mergeConfigs(merged, cfg)
	}
	return merged, nil
}
