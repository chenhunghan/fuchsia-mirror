// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package botanist

import (
	"errors"
	"fmt"
	"testing"
)

func TestPhases(t *testing.T) {
	seen := make(map[string]Phase)
	for _, p := range AllPhases() {
		if !p.IsValid() {
			t.Errorf("AllPhases() member %d reported IsValid() = false", p)
		}
		name := p.String()
		if name == "" {
			t.Errorf("AllPhases() member %d returned empty String()", p)
		}
		if prev, dup := seen[name]; dup {
			t.Errorf("duplicate phase name %q for %d and %d", name, prev, p)
		}
		seen[name] = p
	}

	for _, invalid := range []Phase{PhaseUnknown, Phase(-1), Phase(999)} {
		if invalid.IsValid() {
			t.Errorf("Phase(%d).IsValid() = true, want false", invalid)
		}
		if got := invalid.String(); got != "" {
			t.Errorf("Phase(%d).String() = %q, want empty string", invalid, got)
		}
	}
}

func TestNewPhaseError(t *testing.T) {
	if err := NewPhaseError(PhaseTargetBoot, nil); err != nil {
		t.Fatalf("NewPhaseError(PhaseTargetBoot, nil) = %v, want nil", err)
	}

	baseErr := errors.New("underlying cause")

	tests := []struct {
		name         string
		phase        Phase
		err          error
		wantHasPhase bool
		wantPhase    Phase
	}{
		{
			name:         "tags raw error with valid phase",
			phase:        PhaseTargetConnectivity,
			err:          fmt.Errorf("ssh failed: %w", baseErr),
			wantHasPhase: true,
			wantPhase:    PhaseTargetConnectivity,
		},
		{
			name:         "leaves error untagged for invalid phase",
			phase:        PhaseUnknown,
			err:          baseErr,
			wantHasPhase: false,
		},
		{
			name:         "preserves existing valid PhaseError in chain over outer phase",
			phase:        PhaseTargetBoot,
			err:          errors.Join(errors.New("cleanup error"), NewPhaseError(PhaseTargetProvisioning, baseErr)),
			wantHasPhase: true,
			wantPhase:    PhaseTargetProvisioning,
		},
		{
			name:         "replaces invalid inner PhaseError with valid outer phase",
			phase:        PhaseTargetBoot,
			err:          &PhaseError{phase: PhaseUnknown, err: baseErr},
			wantHasPhase: true,
			wantPhase:    PhaseTargetBoot,
		},
	}

	for _, tc := range tests {
		t.Run(tc.name, func(t *testing.T) {
			err := NewPhaseError(tc.phase, tc.err)
			if !errors.Is(err, baseErr) {
				t.Errorf("errors.Is(err, baseErr) = false; err = %v", err)
			}
			if got, want := err.Error(), tc.err.Error(); got != want {
				t.Errorf("Error() = %q, want %q", got, want)
			}

			var pErr *PhaseError
			hasPhase := errors.As(err, &pErr)
			if hasPhase != tc.wantHasPhase {
				t.Fatalf("errors.As(err, &PhaseError) = %v, want %v", hasPhase, tc.wantHasPhase)
			}
			if tc.wantHasPhase && pErr.Phase() != tc.wantPhase {
				t.Errorf("Phase() = %v, want %v", pErr.Phase(), tc.wantPhase)
			}
		})
	}
}

func TestPhaseErrorf(t *testing.T) {
	err := PhaseErrorf(PhaseTargetProvisioning, "flash %s: %w", "slot A", errors.New("timeout"))
	if got, want := err.Error(), "flash slot A: timeout"; got != want {
		t.Errorf("Error() = %q, want %q", got, want)
	}
	var pErr *PhaseError
	if !errors.As(err, &pErr) || pErr.Phase() != PhaseTargetProvisioning {
		t.Errorf("errors.As(err, &pErr) = %v (phase=%v), want PhaseTargetProvisioning", pErr != nil, pErr.Phase())
	}
}

func TestPhaseErrorNilSafety(t *testing.T) {
	for _, tc := range []struct {
		name string
		err  *PhaseError
	}{
		{name: "nil receiver", err: nil},
		{name: "zero-value struct", err: &PhaseError{}},
	} {
		t.Run(tc.name, func(t *testing.T) {
			if got := tc.err.Error(); got != "<nil>" {
				t.Errorf("Error() = %q, want <nil>", got)
			}
			if got := tc.err.Phase(); got != PhaseUnknown {
				t.Errorf("Phase() = %v, want PhaseUnknown", got)
			}
			if got := tc.err.Unwrap(); got != nil {
				t.Errorf("Unwrap() = %v, want nil", got)
			}
		})
	}
}
