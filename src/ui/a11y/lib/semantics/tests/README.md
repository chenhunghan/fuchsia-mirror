# `semantics manager tests`

## Running the tests

To run all of the semantics tests, build with a configuration such as
```
fx set minimal.<board> --with //src/ui/a11y/bundles:tests
```

And run the tests using
```
# Run the integration tests
fx test web-semantics-test
# Run the unit tests
fx test semantics-tests-component
```

## Integration test internals

The web semantics integration tests launch an isolated UI test realm (`UITestManager`) and
Chromium `ContextProvider`, and provide a `fuchsia.accessibility.semantics.SemanticsManager`
implementation (`a11y::ViewManager`) used to inspect semantic trees and trigger semantic
actions on HTML pages loaded into a `fuchsia.web.Frame`.
