// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <Python.h>

#include "build/bazel/rules/python/tests/c_extension/helper.h"
#include "build/bazel/rules/python/tests/c_extension/shared_helper.h"

#ifndef EXT_TEST_DEFINE_VAL
#error "EXT_TEST_DEFINE_VAL must be set via python_c_extension defines"
#endif

namespace {

PyObject* add_ints(PyObject* self, PyObject* args) {
  int a = 0;
  int b = 0;
  if (!PyArg_ParseTuple(args, "ii", &a, &b)) {
    return nullptr;
  }
  if (a < 0 || b < 0) {
    PyErr_SetString(PyExc_ValueError, "inputs must be non-negative");
    return nullptr;
  }
  return PyLong_FromLongLong(c_ext_static_add(a, b));
}

PyObject* mul_ints(PyObject* self, PyObject* args) {
  int a = 0;
  int b = 0;
  if (!PyArg_ParseTuple(args, "ii", &a, &b)) {
    return nullptr;
  }
  return PyLong_FromLongLong(c_ext_shared_mul(a, b));
}

PyObject* get_define_val(PyObject* self, PyObject* args) {
  return PyLong_FromLong(EXT_TEST_DEFINE_VAL);
}

PyMethodDef kMethods[] = {
    {"add_ints", add_ints, METH_VARARGS, "Add two non-negative ints via static helper."},
    {"mul_ints", mul_ints, METH_VARARGS, "Multiply two ints via shared helper."},
    {"get_define_val", get_define_val, METH_NOARGS, "Return compile-time define value."},
    {nullptr, nullptr, 0, nullptr},
};

struct PyModuleDef kModuleDef = {
    PyModuleDef_HEAD_INIT, "libc_ext_with_stubs", nullptr, -1, kMethods,
};

}  // namespace

PyMODINIT_FUNC PyInit_libc_ext_with_stubs() { return PyModule_Create(&kModuleDef); }
