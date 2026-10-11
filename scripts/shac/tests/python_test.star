# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Tests for the checks in //scripts/shac/python.star."""

load("//scripts/shac/python.star", "_py_shebangs")

_WANT_SHEBANG = "#!/usr/bin/env fuchsia-vendored-python"

def test_py_shebangs():
    res = testing.run(_py_shebangs, files = {
        "foo.py": """\
#!/usr/bin/env python3
import os
""",
    })
    asserts.eq(res.findings, (
        testing.finding(
            level = "warning",
            message = "Use fuchsia-vendored-python in shebangs for Python scripts.",
            filepath = "foo.py",
            line = 1,
            replacements = [_WANT_SHEBANG + "\n"],
        ),
    ))
    asserts.eq(res.files["foo.py"], _WANT_SHEBANG + """
import os
""")

def test_py_shebangs_ok():
    res = testing.run(_py_shebangs, files = {
        "correct.py": _WANT_SHEBANG + "\n",
        "no_shebang.py": "import os\n",
        "empty.py": "",
        "opt_out.py": """\
#!/usr/bin/env python3
# allow-non-vendored-python
""",
        "build/bazel/foo.py": "#!/usr/bin/env python3\n",
        "third_party/foo.py": "#!/usr/bin/env python3\n",
        "foo.sh": "#!/bin/bash\n",
    })
    asserts.eq(res.findings, ())
