# lib/lockup_detector -- library for detecting kernel lockups

This library provides instrumentation for detecting different kinds of
lockups.

The library has two tools, a critical section detector and a CPU
heartbeat checker.

## Critical Section Detector

The critical section detector detects when the kernel has remained in
a critical section for "too long".  Critical sections are marked using
`lockup_timed_begin()` and `lockup_timed_end()`.  The code executing
during an SMC call is an example of a critical section.  A function
that temporarily disables interrupts would be another example.

### Self Checking and Cross Checking

Currently, detection is performed in two ways.

First, each time a CPU leaves a critical section (calls
`lockup_timed_end()`), it will observe the time spent in the critical
section, and then update kcounters which track the number of long
running critical sections we have seen.  Additionally, it will track
the "worst case" critical section time for that CPU.

Second, If a CPU calls `lockup_timed_begin()`, but never calls
`lockup_timed_end()`, and the CPU has spent more than the configured
threshold amount of time in the critical section, a lockup will be
reported as a `KERNEL_OOPS` by another one of the CPUs when it is
performing a heartbeat check.  If the amount of time spent by the
locked-up CPU in the critical section exceeds the fatal threshold, the
kernel will generate a crashlog and reboot, indicating a reboot reason
of `SOFTWARE_WATCHDOG` in the crashlog as it does.

The `k lockup status` command can be used to check if a CPU is
currently in a critical section, and to print the current worst case
critical section times for each CPU.

### Threshold

Choosing appropriate thresholds for the critical section detector is
important.  The values should be small enough to detect performance
impacting lockups, but large enough to avoid false alarms, especially
when running on virtualized hardware.  Setting both of these values to
0 will completely disable critical section lockup detection.

See also `kernel.lockup-detector.critical-section-threshold-ms` and
`kernel.lockup-detector.critical-section-fatal-threshold-ms`.

## Heartbeak Checker

The heartbeat checker is used to detect when a CPU has stopped
responding to interrupts.

When enabled, all CPUs will run a periodic timer callback that emits a
"heartbeat" by updating a timestamp in its per CPU structure.
Afterwards, they will check each of the other CPUs' last heartbeat and
if the last heartbeat is older than the configured threshold, a (rate
limited) `KERNEL_OOPS` will be emitted.  If the time since last
heartbeat exceeds the fatal threshold, a crashlog will be generated
and the kernel will reboot, indicating a reboot reason of
`SOFTWARE_WATCHDOG` in the crashlog as it does.

`kernel.lockup-detector.heartbeat-period-ms` controls how frequently
the CPUs emit heartbeats and perform checks.

`kernel.lockup-detector.heartbeat-age-threshold-ms` controls how long
a CPU can go without emitting a heartbeat before it is considered to
be locked up and a `KERNEL_OOPS` is generated.

`kernel.lockup-detector.heartbeat-age-fatal-threshold-ms` controls how
long a CPU can go without emitting a heartbeat before a
`SOFTWARE_WATCHDOG` reboot is triggered.

## Fair Starvation Checker

The fair starvation checker reports when a CPU runs only deadline
threads for a long time while fair threads are queued on it.  That
happens when a deadline thread keeps the CPU to itself, for example by
polling in a loop for something only a fair thread on that CPU can
provide, and when deadline load exceeds what the CPU can currently
supply (for example on a thermally limited core).

The scheduler keeps a per-CPU starvation clock: it starts when a
deadline thread is selected while fair threads are queued, and stops
when a fair thread is selected or none is waiting.  Each CPU's heartbeat
callback checks its own clock.  When the clock exceeds the threshold, it
names the deadline thread that ran for more than half of the episode, if
there is one, and prints the running thread and the queued fair threads
with their ids and affinity masks.  When several deadline threads share
the CPU and none ran for more than half of the episode, as on an
overloaded CPU, the report names none.  An episode that continues is
reported again every 10 thresholds.

`kernel.lockup-detector.fair-starvation-threshold-ms` enables the check
and sets the threshold.  It is disabled by default.  Because reports are
made from the heartbeat, fair threads are starved for the threshold
rounded up to the next heartbeat: with a 2000 ms threshold and a 1000 ms
heartbeat, a report comes 2000 to 3000 ms into an episode.  A threshold
shorter than the heartbeat period has no effect of its own, so it is
raised to the heartbeat period with a warning at boot.  To bound
starvation more tightly, shorten the heartbeat period.

`kernel.lockup-detector.fair-starvation-exception` additionally raises a
policy exception on the thread that the report names, if it is a user
thread and is the one running when the report is made, so that the
exception handler can record where the thread was executing.  `crashsvc`
prints the thread and resumes it.  The exception's code is
`ZX_EXCP_POLICY_CODE_SPIN_DETECTION` and its data is the CPU number.  A
thread that already has a policy exception pending for a violation does
not get one.

`kernel.lockup-detector.fair-starvation-relief-ms` enables relief: after
a report, the CPU guarantees fair threads a share of its time
(`kernel.lockup-detector.fair-starvation-relief-percent`, measured over
windows of `kernel.lockup-detector.fair-starvation-relief-window-ms`)
for that long, ahead of deadline threads.  In each window fair threads
run first until they have had their share, which delays deadline threads
on that CPU by up to the share of one window at a time: 1 ms with the
defaults.  The delay can be up to twice that across a window boundary,
when fair threads take their share at the end of one window and at the
start of the next.  A running deadline thread is preempted at the end
of each window, so the share does not depend on how long deadline
threads run.  A deadline thread whose period is not much longer than
the delay can lose a whole period to it, so a product with such threads
should shorten the window.  The share goes to the fair class as a whole;
which fair thread gets it is decided by the fair scheduler, and a fair
thread that never blocks can take nearly all of it.  Relief is a safety
net, not a level of service.  Relief only ever follows a report, and
nothing is reported during relief; detection starts over when it ends.
Fair threads are deliberately not protected before the threshold is
reached, so that the starvation stays visible and its cause can be
fixed.
Deadline threads on that CPU can miss deadlines during relief.

## Future Directions

A future version of this library may add instrumentation for detecting
when a CPU has been "executing kernel code for too long", or has been
in a hypervisor or Secure Monitor call for too long.
