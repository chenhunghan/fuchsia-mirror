Run this component to generate a 100% -> 0% -> 100% alternating load pattern on a device.


Usage:

```
fx set ... --with //src/tests/end_to_end/power/load_generator
fx serve # In background shell

ffx component run /core/ffx-laboratory:load_generator \
  fuchsia-pkg://fuchsia.com/load_generator#meta/load_generator.cm
ffx component config set --reload /core/ffx-laboratory:load_generator \
  "load_pattern=100,200,300,400,500"
```

Where `load_pattern` is the duration in milliseconds of the alternating load pattern. For example
`100,200,300,400,500` will generate 100ms of 100% load, produce no load for 200ms, then produce
100% load for 300ms and so on.
