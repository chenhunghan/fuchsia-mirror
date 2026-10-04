// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#[cfg(feature = "tracing")]
pub use fuchsia_trace::{
    Id, Scope, TraceFutureExt, counter, duration, flow_begin, flow_end, flow_step, instant,
    trace_future_args,
};

#[cfg(not(feature = "tracing"))]
mod noop;
#[cfg(not(feature = "tracing"))]
pub mod __backend {
    pub use crate::noop::*;
}
#[cfg(not(feature = "tracing"))]
pub use crate::noop::{Id, Scope, TraceFutureExt};

#[cfg(test)]
mod tests {
    use crate::*;

    #[fuchsia::test]
    fn test_duration() {
        let trace_only_var = 6;
        duration!("category", "name");
        duration!("category", "name", "arg" => 5);
        duration!("category", "name", "arg" => 5, "arg2" => trace_only_var);
    }

    #[fuchsia::test]
    fn test_instant() {
        let trace_only_var = 6;
        instant!("category", "name", Scope::Thread);
        instant!("category", "name", Scope::Thread, "arg" => 5);
        instant!("category", "name", Scope::Thread, "arg" => 5, "arg2" => trace_only_var);
    }

    #[fuchsia::test]
    fn test_flow_begin() {
        let trace_only_var = 6;
        let flow_id: Id = u64::from(Id::new()).into();
        flow_begin!("category", "name", flow_id);
        flow_begin!("category", "name", flow_id, "arg" => 5);
        flow_begin!("category", "name", flow_id, "arg" => 5, "arg2" => trace_only_var);
    }

    #[fuchsia::test]
    fn test_flow_step() {
        let trace_only_var = 6;
        let flow_id: Id = 5u64.into();
        flow_step!("category", "name", flow_id);
        flow_step!("category", "name", flow_id, "arg" => 5);
        flow_step!("category", "name", flow_id, "arg" => 5, "arg2" => trace_only_var);
    }

    #[fuchsia::test]
    fn test_flow_end() {
        let trace_only_var = 6;
        let flow_id: Id = 5u64.into();
        flow_end!("category", "name", flow_id);
        flow_end!("category", "name", flow_id, "arg" => 5);
        flow_end!("category", "name", flow_id, "arg" => 5, "arg2" => trace_only_var);
    }

    #[fuchsia::test]
    async fn test_trace_future() {
        let value = async move { 5 }.trace(trace_future_args!("category", "name")).await;
        assert_eq!(value, 5);

        let value =
            async move { 5 }.trace(trace_future_args!("category", "name", "arg1" => 6)).await;
        assert_eq!(value, 5);

        let trace_only_var = 7;
        let value = async move { 5 }
            .trace(trace_future_args!("category", "name", "arg1" => 6, "ar2" => trace_only_var))
            .await;
        assert_eq!(value, 5);
    }

    #[fuchsia::test]
    fn test_arg_types() {
        duration!("category", "name", "bool" => true);
        duration!("category", "name", "i32" => 5i32, "u32" => 5u32);
        duration!("category", "name", "i64" => 5i64, "u64" => 5u64);
        duration!("category", "name", "isize" => 5isize, "usize" => 5usize);
        duration!("category", "name", "f64" => 5f64);

        let owned_str = "test-str".to_owned();
        duration!("category", "name", "str" => owned_str.as_str());

        let mut value = 5u64;
        duration!("category", "name", "const-ptr" => &value as *const u64);
        duration!("category", "name", "mut-ptr" => &mut value as *mut u64);
    }

    #[fuchsia::test]
    fn test_counter() {
        counter!("category", "name", 1, "a" => 10);
        counter!("category", "name", 1, "a" => 10, "b" => 20);
    }

    #[fuchsia::test]
    fn test_id_traits() {
        use std::collections::HashSet;

        let id1 = Id::new();
        let id2 = Id::from(42u64);
        let id3 = id2;
        assert_eq!(id2, id3);
        assert!(id1 <= id2 || id2 <= id1);
        assert!(!format!("{id1:?}").is_empty());

        let mut set = HashSet::new();
        set.insert(id2);
        assert!(set.contains(&id3));
    }

    #[cfg(not(feature = "tracing"))]
    #[fuchsia::test]
    fn test_noop_backend() {
        use crate::__backend::ArgValue;

        let id = Id::new();
        let raw: u64 = id.into();
        assert_eq!(raw, 0);

        __backend::use_duration_args("category", "name");
        __backend::use_instant_args("category", "name", Scope::Process);
        __backend::use_instant_args("category", "name", Scope::Global);
        __backend::use_flow_args("category", "name", id);
        __backend::use_counter_args("category", "name", 1);

        let mut val = 42u64;
        let s = "hello";
        __backend::use_args(&[
            <()>::of("unit", ()),
            bool::of("bool", true),
            i32::of("i32", -1),
            u32::of("u32", 1),
            i64::of("i64", -2),
            u64::of("u64", 2),
            isize::of("isize", -3),
            usize::of("usize", 3),
            f64::of("f64", 1.5),
            <&str>::of("str", s),
            <*const u64>::of("cptr", &val as *const u64),
            <*mut u64>::of("mptr", &mut val as *mut u64),
        ]);
    }
}
