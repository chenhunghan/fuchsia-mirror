Verifies that the device suspends and resumes at the appropriate times.

### Running the test locally

When run at a desk without an automated USB disconnector (such as `DMC_PATH` in
infra), the test logs instructions when you should manually unplug and replug
the USB cable, and uses more forgiving timeouts to give you time to do so:

```sh
fx add-host-test //src/tests/end_to_end/power/suspend_resume:suspend_resume_test
fx test //src/tests/end_to_end/power/suspend_resume:suspend_resume_test --e2e --output
```