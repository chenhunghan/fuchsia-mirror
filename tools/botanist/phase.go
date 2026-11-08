// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package botanist

import (
	"errors"
	"fmt"
)

// Phase represents a canonical lifecycle phase in botanist.
type Phase int

const (
	// PhaseUnknown indicates that no valid lifecycle phase was assigned.
	PhaseUnknown Phase = iota
	PhaseEnvironmentAndTools
	PhaseTargetProvisioning
	PhaseTargetBoot
	PhaseTargetConnectivity
	PhaseTestExecution
)

// String returns the canonical snake_case string representation of the phase,
// or an empty string for PhaseUnknown or any invalid phase value.
// LINT.IfChange(canonical_phases)
func (p Phase) String() string {
	switch p {
	case PhaseEnvironmentAndTools:
		return "environment_and_tools"
	case PhaseTargetProvisioning:
		return "target_provisioning"
	case PhaseTargetBoot:
		return "target_boot"
	case PhaseTargetConnectivity:
		return "target_connectivity"
	case PhaseTestExecution:
		return "test_execution"
	default:
		return ""
	}
}

// LINT.ThenChange(/tools/testing/tefmocheck/string_in_log_check.go:canonical_phases)

// IsValid returns true if the phase is one of the recognized canonical phases.
func (p Phase) IsValid() bool {
	return p.String() != ""
}

// AllPhases returns all canonical lifecycle phases in progression order.
func AllPhases() []Phase {
	return []Phase{
		PhaseEnvironmentAndTools,
		PhaseTargetProvisioning,
		PhaseTargetBoot,
		PhaseTargetConnectivity,
		PhaseTestExecution,
	}
}

// PhaseError is an error associated with a canonical botanist lifecycle phase.
type PhaseError struct {
	phase Phase
	err   error
}

func (e *PhaseError) Error() string {
	if e == nil || e.err == nil {
		return "<nil>"
	}
	return e.err.Error()
}

// Phase returns the canonical lifecycle phase associated with the error.
func (e *PhaseError) Phase() Phase {
	if e == nil {
		return PhaseUnknown
	}
	return e.phase
}

func (e *PhaseError) Unwrap() error {
	if e == nil {
		return nil
	}
	return e.err
}

// NewPhaseError constructs an error associated with phase wrapping err.
// If err is nil, it returns nil.
// If err already carries a valid *PhaseError in its chain, it is returned as-is to preserve specific sub-phase attribution.
// If phase is invalid (such as PhaseUnknown), err is returned without a PhaseError wrapper.
func NewPhaseError(phase Phase, err error) error {
	if err == nil {
		return nil
	}
	var existing *PhaseError
	if errors.As(err, &existing) && existing.Phase().IsValid() {
		return err
	}
	if !phase.IsValid() {
		return err
	}
	return &PhaseError{
		phase: phase,
		err:   err,
	}
}

// PhaseErrorf formats an error according to a format specifier and associates it with phase.
// If phase is invalid (such as PhaseUnknown), it returns the formatted error without a PhaseError wrapper.
func PhaseErrorf(phase Phase, format string, a ...any) error {
	return NewPhaseError(phase, fmt.Errorf(format, a...))
}
