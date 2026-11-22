The internal
`pest_stack_guard` helper and its dependencies are not built by `pest` without
`std`. Host-side procedural macros can still use the native stack guard when
cross-compiling a parser to another target.