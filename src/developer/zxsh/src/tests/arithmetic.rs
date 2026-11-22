// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::eval::testing::evaluate_arithmetic;
use crate::eval::{ExecutionContext, ShellState};
use bstr::BStr;

fn eval(expr: &[u8], state: &mut ShellState) -> i64 {
    let ctx = ExecutionContext::initial().unwrap();
    evaluate_arithmetic(BStr::new(expr), state, &ctx).unwrap()
}

fn eval_res(expr: &[u8], state: &mut ShellState) -> Result<i64, String> {
    let ctx = ExecutionContext::initial().unwrap();
    evaluate_arithmetic(BStr::new(expr), state, &ctx)
}

#[test]
fn test_basic_arithmetic() {
    let mut state = ShellState::new();

    assert_eq!(eval(b"1 + 2", &mut state), 3);
    assert_eq!(eval(b"5 - 3", &mut state), 2);
    assert_eq!(eval(b"2 * 3", &mut state), 6);
    assert_eq!(eval(b"8 / 2", &mut state), 4);
    assert_eq!(eval(b"7 % 3", &mut state), 1);
}

#[test]
fn test_precedence() {
    let mut state = ShellState::new();

    assert_eq!(eval(b"1 + 2 * 3", &mut state), 7);
    assert_eq!(eval(b"(1 + 2) * 3", &mut state), 9);
    assert_eq!(eval(b"10 - 2 * 3 + 4", &mut state), 8);
    assert_eq!(eval(b"10 / 2 * 3", &mut state), 15);
}

#[test]
fn test_unary_operators() {
    let mut state = ShellState::new();

    assert_eq!(eval(b"-5", &mut state), -5);
    assert_eq!(eval(b"+5", &mut state), 5);
    assert_eq!(eval(b"--5", &mut state), 5);
    assert_eq!(eval(b"~0", &mut state), -1);
    assert_eq!(eval(b"!0", &mut state), 1);
    assert_eq!(eval(b"!5", &mut state), 0);
}

#[test]
fn test_comparison_operators() {
    let mut state = ShellState::new();

    assert_eq!(eval(b"5 == 5", &mut state), 1);
    assert_eq!(eval(b"5 == 6", &mut state), 0);
    assert_eq!(eval(b"5 != 6", &mut state), 1);
    assert_eq!(eval(b"5 != 5", &mut state), 0);
    assert_eq!(eval(b"5 < 6", &mut state), 1);
    assert_eq!(eval(b"6 < 5", &mut state), 0);
    assert_eq!(eval(b"5 <= 5", &mut state), 1);
    assert_eq!(eval(b"5 > 4", &mut state), 1);
    assert_eq!(eval(b"5 >= 5", &mut state), 1);
}

#[test]
fn test_bitwise_operators() {
    let mut state = ShellState::new();

    assert_eq!(eval(b"5 & 3", &mut state), 1); // 101 & 011 = 001
    assert_eq!(eval(b"5 | 3", &mut state), 7); // 101 | 011 = 111
    assert_eq!(eval(b"5 ^ 3", &mut state), 6); // 101 ^ 011 = 110
    assert_eq!(eval(b"1 << 3", &mut state), 8);
    assert_eq!(eval(b"8 >> 2", &mut state), 2);
}

#[test]
fn test_logical_operators() {
    let mut state = ShellState::new();

    assert_eq!(eval(b"5 && 3", &mut state), 1);
    assert_eq!(eval(b"5 && 0", &mut state), 0);
    assert_eq!(eval(b"0 && 3", &mut state), 0);
    assert_eq!(eval(b"5 || 3", &mut state), 1);
    assert_eq!(eval(b"5 || 0", &mut state), 1);
    assert_eq!(eval(b"0 || 0", &mut state), 0);
}

#[test]
fn test_conditional_operator() {
    let mut state = ShellState::new();

    assert_eq!(eval(b"1 ? 10 : 20", &mut state), 10);
    assert_eq!(eval(b"0 ? 10 : 20", &mut state), 20);
    assert_eq!(eval(b"1 + 1 ? 5 * 2 : 3 * 3", &mut state), 10);
}

#[test]
fn test_variables_and_assignment() {
    let mut state = ShellState::new();

    state.set_var(b"A", b"5");
    assert_eq!(eval(b"A + 1", &mut state), 6);
    assert_eq!(eval(b"B + 1", &mut state), 1); // Unset is 0

    assert_eq!(eval(b"C = 10", &mut state), 10);
    assert_eq!(state.get_var(b"C").unwrap(), "10");

    assert_eq!(eval(b"C += 5", &mut state), 15);
    assert_eq!(state.get_var(b"C").unwrap(), "15");

    assert_eq!(eval(b"C -= 3", &mut state), 12);
    assert_eq!(eval(b"C *= 2", &mut state), 24);
    assert_eq!(eval(b"C /= 4", &mut state), 6);
    assert_eq!(eval(b"C %= 4", &mut state), 2);

    state.set_var(b"D", b"2");
    assert_eq!(eval(b"D <<= 2", &mut state), 8);
    assert_eq!(eval(b"D >>= 1", &mut state), 4);

    state.set_var(b"E", b"5");
    assert_eq!(eval(b"E &= 3", &mut state), 1);
    assert_eq!(eval(b"E |= 6", &mut state), 7);
    assert_eq!(eval(b"E ^= 3", &mut state), 4);
}

#[test]
fn test_nested_variables() {
    let mut state = ShellState::new();

    state.set_var(b"A", b"B + 1");
    state.set_var(b"B", b"5");
    assert_eq!(eval(b"A + 1", &mut state), 7);
}

#[test]
fn test_errors() {
    let mut state = ShellState::new();

    assert!(eval_res(b"5 / 0", &mut state).is_err());
    assert!(eval_res(b"5 % 0", &mut state).is_err());
    assert!(eval_res(b"C /= 0", &mut state).is_err());
    assert!(eval_res(b"C %= 0", &mut state).is_err());

    // Loop detection
    state.set_var(b"A", b"B");
    state.set_var(b"B", b"A");
    assert!(eval_res(b"A", &mut state).is_err());

    // Bad assignment LHS
    assert!(eval_res(b"5 = 10", &mut state).is_err());
    assert!(eval_res(b"1 + 2 = 10", &mut state).is_err());
}

#[test]
fn test_arithmetic_invalid_byte() {
    let mut state = ShellState::new();
    let res = eval_res(b"1 + @", &mut state);
    assert_eq!(res, Err("Invalid byte in arithmetic expression: 0x40".to_string()));
}

#[test]
fn test_arithmetic_unexpected_end_unary() {
    let mut state = ShellState::new();
    let res = eval_res(b"1 + -", &mut state);
    assert_eq!(res, Err("Unexpected end of expression".to_string()));
}

#[test]
fn test_arithmetic_unexpected_end_factor() {
    let mut state = ShellState::new();
    let res = eval_res(b"1 +", &mut state);
    assert_eq!(res, Err("Unexpected end of expression".to_string()));
}

#[test]
fn test_arithmetic_nounset_error() {
    let mut state = ShellState::new();
    state.set_option_by_name(BStr::new(b"nounset"), true).unwrap();
    let res = eval_res(b"UNSET_VAR + 1", &mut state);
    assert_eq!(res, Err("UNSET_VAR: parameter not set".to_string()));
}

#[test]
fn test_arithmetic_unclosed_paren() {
    let mut state = ShellState::new();
    let res = eval_res(b"(1 + 2", &mut state);
    assert_eq!(res, Err("Expected matching ')' in arithmetic expression".to_string()));
}

#[test]
fn test_arithmetic_unexpected_token() {
    let mut state = ShellState::new();
    let res = eval_res(b")", &mut state);
    assert_eq!(res, Err("Unexpected token in factor: RParen".to_string()));
}

#[test]
fn test_arithmetic_recursion_limit() {
    let mut state = ShellState::new();
    for i in 0..33 {
        let name = format!("V{}", i);
        let val = format!("V{}", i + 1);
        state.set_var(BStr::new(name.as_bytes()), BStr::new(val.as_bytes()));
    }
    let res = eval_res(b"V0", &mut state);
    assert_eq!(res, Err("Recursion limit exceeded in variable arithmetic expansion".to_string()));
}

#[test]
fn test_arithmetic_missing_colon_in_ternary() {
    let mut state = ShellState::new();
    let res = eval_res(b"1 ? 2", &mut state);
    assert_eq!(res, Err("Expected ':' in conditional expression".to_string()));
}
#[test]
fn test_arithmetic_overflow() {
    let mut state = ShellState::new();
    assert_eq!(eval(b"9223372036854775807 + 1", &mut state), i64::MIN);
    assert_eq!(eval(b"-9223372036854775808 - 1", &mut state), i64::MAX);
    assert_eq!(eval(b"9223372036854775807 * 2", &mut state), -2);
    assert_eq!(eval(b"-(-9223372036854775808)", &mut state), i64::MIN);
    assert_eq!(eval(b"-9223372036854775808 / -1", &mut state), i64::MIN);
    eval(b"X = 9223372036854775807", &mut state);
    assert_eq!(eval(b"X += 1", &mut state), i64::MIN);
}

#[test]
fn test_empty_and_trailing_tokens() {
    let mut state = ShellState::new();
    assert_eq!(eval(b"", &mut state), 0);
    assert_eq!(eval(b"   \t\n  ", &mut state), 0);
    assert_eq!(eval(b"$UNSET", &mut state), 0);

    assert!(eval_res(b"1 2", &mut state).is_err());
    assert!(eval_res(b"1 )", &mut state).is_err());
}

#[test]
fn test_hex_and_octal_constants() {
    let mut state = ShellState::new();

    // Hexadecimal
    assert_eq!(eval(b"0x0", &mut state), 0);
    assert_eq!(eval(b"0x10", &mut state), 16);
    assert_eq!(eval(b"0Xff", &mut state), 255);
    assert_eq!(eval(b"0x1a2B", &mut state), 0x1a2b);
    assert_eq!(eval(b"-0x10", &mut state), -16);
    assert_eq!(eval(b"+0x10", &mut state), 16);

    // Octal
    assert_eq!(eval(b"0", &mut state), 0);
    assert_eq!(eval(b"00", &mut state), 0);
    assert_eq!(eval(b"010", &mut state), 8);
    assert_eq!(eval(b"077", &mut state), 63);
    assert_eq!(eval(b"-010", &mut state), -8);
    assert_eq!(eval(b"+010", &mut state), 8);

    // Variable holding hex/octal with sign
    state.set_var(b"HEX_VAR", b"-0x20");
    state.set_var(b"OCT_VAR", b"+020");
    assert_eq!(eval(b"HEX_VAR + OCT_VAR", &mut state), -16);

    // Invalid integer constants
    for bad in [b"08" as &[u8], b"09", b"0x", b"0X", b"0x1g", b"12abc", b"1_0"] {
        assert_eq!(
            eval_res(bad, &mut state),
            Err("invalid integer constant".to_string()),
            "expected invalid integer constant for {:?}",
            BStr::new(bad)
        );
    }
}

#[test]
fn test_assignment_nounset_and_readonly() {
    let mut state = ShellState::new();
    state.opt_nounset = true;

    // Simple `=` assignment to unset variable succeeds under `set -u`
    assert_eq!(eval(b"UNSET_ASSIGN = 42", &mut state), 42);
    assert_eq!(state.get_var(b"UNSET_ASSIGN").unwrap(), "42");

    // Simple `=` assignment overwrites variable without evaluating its previous invalid/recursive value
    state.set_var(b"SELF_LOOP", b"SELF_LOOP");
    assert_eq!(eval(b"SELF_LOOP = 7", &mut state), 7);
    assert_eq!(state.get_var(b"SELF_LOOP").unwrap(), "7");

    // Compound assignment `+=` to unset variable fails under `set -u`
    assert_eq!(
        eval_res(b"UNSET_COMPOUND += 5", &mut state),
        Err("UNSET_COMPOUND: parameter not set".to_string())
    );
    assert_eq!(
        eval_res(b"UNSET_READ", &mut state),
        Err("UNSET_READ: parameter not set".to_string())
    );

    // Readonly variable assignment fails
    state.opt_nounset = false;
    state.set_var(b"RO_VAR", b"10");
    state.make_readonly(BStr::new(b"RO_VAR"));
    assert_eq!(eval_res(b"RO_VAR = 5", &mut state), Err("RO_VAR: is read only".to_string()));
    assert_eq!(eval_res(b"RO_VAR += 5", &mut state), Err("RO_VAR: is read only".to_string()));
    assert_eq!(state.get_var(b"RO_VAR").unwrap(), "10");
}

#[test]
fn test_short_circuit_evaluation() {
    let mut state = ShellState::new();

    // `||` short-circuits when LHS is non-zero
    assert_eq!(eval(b"1 || (X = 99)", &mut state), 1);
    assert!(state.get_var(b"X").is_none());
    assert_eq!(eval(b"1 || (1 / 0)", &mut state), 1);
    assert_eq!(eval(b"1 || (1 % 0)", &mut state), 1);
    assert_eq!(eval(b"0 || (X = 9) || (Y = 10)", &mut state), 1);
    assert_eq!(state.get_var(b"X").unwrap(), "9");
    assert!(state.get_var(b"Y").is_none());

    // `&&` short-circuits when LHS is zero
    assert_eq!(eval(b"0 && (Z = 88)", &mut state), 0);
    assert!(state.get_var(b"Z").is_none());
    assert_eq!(eval(b"0 && (1 / 0)", &mut state), 0);
    assert_eq!(eval(b"0 && (1 % 0)", &mut state), 0);
    assert_eq!(eval(b"1 && (Z = 7)", &mut state), 1);
    assert_eq!(state.get_var(b"Z").unwrap(), "7");

    // `? :` short-circuits unselected branch
    assert_eq!(eval(b"1 ? 10 : (1 / 0)", &mut state), 10);
    assert_eq!(eval(b"0 ? (1 / 0) : 20", &mut state), 20);
    assert_eq!(eval(b"1 ? (T = 11) : (F = 22)", &mut state), 11);
    assert_eq!(state.get_var(b"T").unwrap(), "11");
    assert!(state.get_var(b"F").is_none());
    assert_eq!(eval(b"0 ? (T2 = 11) : (F2 = 22)", &mut state), 22);
    assert!(state.get_var(b"T2").is_none());
    assert_eq!(state.get_var(b"F2").unwrap(), "22");

    // Short-circuited branch does not trigger `set -u` error, but still validates syntax
    state.opt_nounset = true;
    assert_eq!(eval(b"1 || NEVER_SET", &mut state), 1);
    assert_eq!(eval(b"0 && NEVER_SET", &mut state), 0);
    assert_eq!(eval(b"1 ? 5 : NEVER_SET", &mut state), 5);
    assert!(eval_res(b"1 || (1 = 2)", &mut state).is_err());
    assert!(eval_res(b"0 && (1 ? 2)", &mut state).is_err());
}
