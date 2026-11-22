// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#define Py_LIMITED_API 0x030b00f0
#define PY_SSIZE_T_CLEAN
#include <Python.h>

namespace {

PyObject* ping(PyObject* self, PyObject* args) { return PyUnicode_FromString("pong"); }

PyMethodDef kMethods[] = {
    {"ping", ping, METH_NOARGS, "Return 'pong'."},
    {nullptr, nullptr, 0, nullptr},
};

struct PyModuleDef kModuleDef = {
    PyModuleDef_HEAD_INIT, "libc_ext_minimal", nullptr, -1, kMethods,
};

}  // namespace

PyMODINIT_FUNC PyInit_libc_ext_minimal() { return PyModule_Create(&kModuleDef); }
