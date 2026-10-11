# Copyright 2025 The Fuchsia Authors
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""This class is where error information will be stored.
"""

# mypy: disable-error-code="no-untyped-def"
from mobly import signals


class ActsError(signals.TestError):
    """Base Acts Error"""

    def __init__(self, *args, **kwargs):
        class_name = self.__class__.__name__
        self.error_doc = self.__class__.__doc__
        self.error_code = getattr(
            ActsErrorCode, class_name, ActsErrorCode.UNKNOWN
        )
        extras = dict(
            **kwargs, error_doc=self.error_doc, error_code=self.error_code
        )
        details = args[0] if len(args) > 0 else ""
        super().__init__(details, extras)


class ActsErrorCode:
    # Framework Errors 0-999

    UNKNOWN = 0

    # This error code is used to implement unittests for this class.
    ActsError = 100
