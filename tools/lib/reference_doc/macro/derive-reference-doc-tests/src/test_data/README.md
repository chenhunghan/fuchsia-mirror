# CML Refdoc Golden

`//tools/lib/reference_doc/macro/derive-reference-doc-tests:cml-refdoc-golden_test`
checks that `cml_refdoc.golden.md` matches the output of
`cmc print-cml-reference`. To update it:

```sh
fx bazel run --config=host //tools/lib/reference_doc/macro/derive-reference-doc-tests:cml-refdoc-golden_test -- --update
```
