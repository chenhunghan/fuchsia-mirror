# FIDL Coding Tables Tests

The FIDL compiler generates coding tables, which are instructions for the walker/visitor
to encode/decode messages. This test suite runs the FIDL compiler at compile time and
verifies important properties of the generated coding table at run time. Its purpose
is to test the tables generator in fidlc in a flexible manner.

For example, given the following FIDL definition:

```fidl
library example;
flexible union MyMessage {
    int32 a;
    int64 b;
    SomeStruct c;
};
```

It is expected to generate coding tables along the lines of:

```cpp
static const ::fidl::FidlUnionField example_MyMessageFields[] = {
    ::fidl::FidlUnionField(&fidl::internal::kInt32Table, [... union ordinal for a ...]),
    ::fidl::FidlUnionField(&fidl::internal::kInt64Table, [... union ordinal for b ...]),
    ::fidl::FidlUnionField(&example_SomeStructTable, [... union ordinal for c ...])
};
const fidl_type_t example_MyMessageTable =
    fidl_type_t(::fidl::FidlCodedUnion(3, example_MyMessageFields, "example/MyMessage"));
```

The test will check that the pointers in the coding tables are as expected.
