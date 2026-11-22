#!/usr/bin/env fuchsia-vendored-python
# Copyright 2024 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""
Returns graphics metrics in a JSON file.

# Prerequisites:

* Ensure that you have the label
`//src/performance/lib/trace_processing:run_graphics_metrics` in your
`host_labels` in `$(fx get-build-dir)/args.gn`:

    ```
    host_labels = [
      "//src/performance/lib/trace_processing:run_graphics_metrics",
    ]
    ```
* `fx build`

# Run

Run as:

```
fx run_graphics_metrics \
    path/to/trace/file \
    path/to/output/file.fuchsiaperf.json
```

# Print the output

Example below.

```
$ jq -r \
  '["Label", "Test-Suite", "Value", "Unit"], (.[] | [.label, .test_suite, .values[0], .unit]) | @tsv' \
  $HOME/tmp/foo/some_file.fuchsiaperf.json \
  | column -t
Label               Test-Suite  Value               Unit
RenderCpuP5         Manual      0.9867073000000001  milliseconds
RenderCpuP25        Manual      1.044023            milliseconds
RenderCpuP50        Manual      1.0817964999999998  milliseconds
RenderCpuP75        Manual      1.12369725          milliseconds
RenderCpuP95        Manual      15.853793750000005  milliseconds
RenderCpuMin        Manual      0.908333            milliseconds
RenderCpuMax        Manual      33.499375           milliseconds
RenderCpuAverage    Manual      2.627292306763285   milliseconds
RenderTotalP5       Manual      4.0638462           milliseconds
RenderTotalP25      Manual      5.30674475          milliseconds
RenderTotalP50      Manual      7.4009115           milliseconds
RenderTotalP75      Manual      20.3377085          milliseconds
RenderTotalP95      Manual      24.368351650000008  milliseconds
RenderTotalMin      Manual      3.313125            milliseconds
RenderTotalMax      Manual      49.200417           milliseconds
RenderTotalAverage  Manual      11.583923586956521  milliseconds
```

"""

# Almost entirely copied from `run_suspend_metrics.py`.
import argparse
import pathlib
from collections.abc import Sequence

from reporting import metrics
from trace_processing.metrics import scenic


def main(argv: Sequence[str] | None = None) -> None:
    """
    Takes in a trace file and writes metrics in fuchsiaperf.json format to
    `output_path`.
    """
    parser = argparse.ArgumentParser()
    parser.add_argument("path_to_trace", type=str)
    parser.add_argument("output_path", type=str)
    args = parser.parse_args(argv)

    trace_results = scenic.ScenicMetricsProcessor().process_metrics_with_fxt(
        args.path_to_trace
    )

    metrics.TestCaseResult.write_fuchsiaperf_json(
        results=trace_results,
        test_suite="Manual",
        output_path=pathlib.Path(args.output_path),
    )


if __name__ == "__main__":
    main()
