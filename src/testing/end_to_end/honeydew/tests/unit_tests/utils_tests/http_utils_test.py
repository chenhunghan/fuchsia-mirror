# Copyright 2023 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Unit tests for honeydew.utils.http_utils.py."""

import json
import time
import unittest
import urllib.error
import urllib.request
from collections.abc import Callable
from http.client import RemoteDisconnected
from typing import Any
from unittest import mock

from honeydew import errors
from honeydew.utils import http_utils
from parameterized import param, parameterized

# pylint: disable=protected-access
_PARAMS: dict[str, Any] = {
    "url": "http://12.34.56.78",
    "data": {
        "key1": "value1",
        "key2": "",
        "key3": {"k.3.1": "v.3.1"},
    },
    "headers": {
        "Content-Type": "application/json; charset=utf-8",
        "Content-Length": 58,  # len(json.dumps(_PARAMS["data"]))
    },
    "timeout": 15,
    "attempts": http_utils._DEFAULTS["ATTEMPTS"] + 5,
    "interval": http_utils._DEFAULTS["INTERVAL"] + 5,
    "exceptions_to_skip": [],
}

_MOCK_ARGS: dict[str, Any] = {
    "urlopen_resp": b'{"id": "", "result": "fuchsia-emulator", "error": null}'
}


def _custom_test_name_func(
    testcase_func: Callable[..., None], _: str, param_arg: param
) -> str:
    """Custom name function method."""
    test_func_name: str = testcase_func.__name__

    params_dict: dict[str, Any] = param_arg.args[0]
    test_label: str = parameterized.to_safe_name(params_dict["label"])

    return f"{test_func_name}_with_{test_label}"


class HttpUtilsTests(unittest.TestCase):
    """Unit tests for honeydew.utils.http_utils.py."""

    @parameterized.expand(
        [
            (
                {
                    "label": "no_optional_params",
                    "url": _PARAMS["url"],
                    "optional_params": {},
                    "urlopen_resp": _MOCK_ARGS["urlopen_resp"],
                },
            ),
            (
                {
                    "label": "data_and_headers",
                    "url": _PARAMS["url"],
                    "optional_params": {
                        "data": _PARAMS["data"],
                        "headers": _PARAMS["headers"],
                    },
                    "urlopen_resp": _MOCK_ARGS["urlopen_resp"],
                },
            ),
            (
                {
                    "label": "data_but_no_headers",
                    "url": _PARAMS["url"],
                    "optional_params": {
                        "data": _PARAMS["data"],
                    },
                    "urlopen_resp": _MOCK_ARGS["urlopen_resp"],
                },
            ),
            (
                {
                    "label": "no_data",
                    "url": _PARAMS["url"],
                    "optional_params": {},
                    "urlopen_resp": _MOCK_ARGS["urlopen_resp"],
                },
            ),
            (
                {
                    "label": "skip_ssl_cert_check",
                    "url": _PARAMS["url"],
                    "optional_params": {
                        "verify_ssl": False,
                    },
                    "urlopen_resp": _MOCK_ARGS["urlopen_resp"],
                },
            ),
            (
                {
                    "label": "all_optional_params",
                    "url": _PARAMS["url"],
                    "optional_params": {
                        "data": _PARAMS["data"],
                        "headers": _PARAMS["headers"],
                        "timeout": _PARAMS["timeout"],
                        "attempts": _PARAMS["attempts"],
                        "interval": _PARAMS["interval"],
                        "exceptions_to_skip": _PARAMS["exceptions_to_skip"],
                        "verify_ssl": False,
                    },
                    "urlopen_resp": _MOCK_ARGS["urlopen_resp"],
                },
            ),
        ],
        name_func=_custom_test_name_func,
    )
    @mock.patch.object(urllib.request, "urlopen", autospec=True)
    def test_send_http_request_success(
        self, parameterized_dict: dict[str, Any], mock_urlopen: mock.Mock
    ) -> None:
        """Test case for http_utils.send_http_request() success case."""

        urlopen_return_value = mock.MagicMock()
        urlopen_return_value.read.return_value = parameterized_dict[
            "urlopen_resp"
        ]
        urlopen_return_value.__enter__.return_value = urlopen_return_value
        mock_urlopen.return_value = urlopen_return_value

        result: dict[str, Any] = http_utils.send_http_request(
            url=parameterized_dict["url"],
            **parameterized_dict["optional_params"],
        )

        expected_output: dict[str, Any] = json.loads(
            parameterized_dict["urlopen_resp"].decode("utf-8")
        )

        self.assertEqual(result, expected_output)

        mock_urlopen.assert_called_once()

    @mock.patch.object(urllib.request, "urlopen", autospec=True)
    def test_send_http_request_raw_response(
        self, mock_urlopen: mock.Mock
    ) -> None:
        """Test case for http_utils.send_http_request() with parse_json=False."""
        urlopen_return_value = mock.MagicMock()
        urlopen_return_value.read.return_value = b"$A0,01"
        urlopen_return_value.__enter__.return_value = urlopen_return_value
        mock_urlopen.return_value = urlopen_return_value

        result: str = http_utils.send_http_request(
            url=_PARAMS["url"],
            parse_json=False,
        )

        self.assertEqual(result, "$A0,01")
        mock_urlopen.assert_called_once()

    @parameterized.expand(
        [
            (
                {
                    "label": "remote_disconnected",
                    "exception": RemoteDisconnected,
                },
            ),
            (
                {
                    "label": "timeout_error",
                    "exception": TimeoutError,
                },
            ),
        ],
        name_func=_custom_test_name_func,
    )
    @mock.patch.object(time, "sleep", autospec=True)
    @mock.patch.object(urllib.request, "urlopen", autospec=True)
    def test_send_http_request_with_exceptions_to_skip(
        self,
        parameterized_dict: dict[str, Any],
        mock_urlopen: mock.Mock,
        mock_sleep: mock.Mock,
    ) -> None:
        """Testcase to make sure http_utils.send_http_request() do not
        fail when it receives an exception that is part of exceptions_to_skip
        input arg"""
        exception = parameterized_dict["exception"]
        mock_urlopen.side_effect = exception
        response: dict[str, Any] = http_utils.send_http_request(
            url=_PARAMS["url"], exceptions_to_skip=(exception,)
        )
        self.assertEqual(response, {})
        mock_urlopen.assert_called_once()
        mock_sleep.assert_not_called()

    @mock.patch.object(time, "sleep", autospec=True)
    @mock.patch.object(
        urllib.request,
        "urlopen",
        side_effect=ConnectionError("some run time error"),
        autospec=True,
    )
    def test_send_http_request_fail_because_of_exception(
        self, mock_urlopen: mock.Mock, mock_sleep: mock.Mock
    ) -> None:
        """Testcase for http_utils.send_http_request() failure case because of
        an exception."""
        with self.assertRaises(errors.HttpRequestError):
            http_utils.send_http_request(
                url=_PARAMS["url"],
                interval=_PARAMS["interval"],
                attempts=_PARAMS["attempts"],
            )

        self.assertEqual(mock_urlopen.call_count, _PARAMS["attempts"])
        self.assertEqual(mock_sleep.call_count, _PARAMS["attempts"] - 1)

    @parameterized.expand(
        [
            (
                {
                    "label": "urlerror_timeout",
                    "side_effect": urllib.error.URLError(TimeoutError()),
                },
            ),
            (
                {
                    "label": "read_timeout",
                    "side_effect": TimeoutError("The read operation timed out"),
                },
            ),
        ],
        name_func=_custom_test_name_func,
    )
    @mock.patch.object(time, "sleep", autospec=True)
    @mock.patch.object(urllib.request, "urlopen", autospec=True)
    def test_timeout_error(
        self,
        parameterized_dict: dict[str, Any],
        mock_urlopen: mock.Mock,
        mock_sleep: mock.Mock,
    ) -> None:
        """Verify send_http_request() retries and raises HttpTimeoutError."""
        timeout_err = parameterized_dict["side_effect"]
        mock_urlopen.side_effect = timeout_err

        with self.assertRaises(errors.HttpTimeoutError) as ctx:
            http_utils.send_http_request(
                url=_PARAMS["url"],
                timeout=_PARAMS["timeout"],
                interval=_PARAMS["interval"],
                attempts=_PARAMS["attempts"],
            )

        self.assertIs(ctx.exception.__cause__, timeout_err)
        self.assertEqual(mock_urlopen.call_count, _PARAMS["attempts"])
        self.assertEqual(mock_sleep.call_count, _PARAMS["attempts"] - 1)

    @mock.patch.object(time, "sleep", autospec=True)
    @mock.patch.object(urllib.request, "urlopen", autospec=True)
    def test_response_read_timeout_error(
        self, mock_urlopen: mock.Mock, mock_sleep: mock.Mock
    ) -> None:
        """Verify send_http_request() retries response.read() timeouts."""
        timeout_err = TimeoutError("The read operation timed out")
        urlopen_return_value = mock.MagicMock()
        urlopen_return_value.read.side_effect = timeout_err
        urlopen_return_value.__enter__.return_value = urlopen_return_value
        mock_urlopen.return_value = urlopen_return_value

        with self.assertRaises(errors.HttpTimeoutError) as ctx:
            http_utils.send_http_request(
                url=_PARAMS["url"],
                timeout=_PARAMS["timeout"],
                interval=_PARAMS["interval"],
                attempts=_PARAMS["attempts"],
            )

        self.assertIs(ctx.exception.__cause__, timeout_err)
        self.assertEqual(mock_urlopen.call_count, _PARAMS["attempts"])
        self.assertEqual(mock_sleep.call_count, _PARAMS["attempts"] - 1)

    @mock.patch.object(time, "sleep", autospec=True)
    @mock.patch.object(urllib.request, "urlopen", autospec=True)
    def test_retry_after_timeout_error(
        self, mock_urlopen: mock.Mock, mock_sleep: mock.Mock
    ) -> None:
        """Verify send_http_request() succeeds after a transient TimeoutError."""
        urlopen_return_value = mock.MagicMock()
        urlopen_return_value.read.return_value = _MOCK_ARGS["urlopen_resp"]
        urlopen_return_value.__enter__.return_value = urlopen_return_value
        mock_urlopen.side_effect = [
            TimeoutError("The read operation timed out"),
            urlopen_return_value,
        ]

        response = http_utils.send_http_request(
            url=_PARAMS["url"],
            timeout=_PARAMS["timeout"],
            interval=_PARAMS["interval"],
            attempts=_PARAMS["attempts"],
        )

        expected_response = json.loads(
            _MOCK_ARGS["urlopen_resp"].decode("utf-8")
        )
        self.assertEqual(response, expected_response)
        self.assertEqual(mock_urlopen.call_count, 2)
        mock_sleep.assert_called_once_with(_PARAMS["interval"])


if __name__ == "__main__":
    unittest.main()
