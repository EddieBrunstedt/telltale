# Telltale

Telltale watches a few things on your machine and in your life, and reports
how much each one needs your attention. Like a telltale on a car dashboard, it
stays dark until something does.

## Language

**Gauge**:
One thing Telltale watches, such as network traffic, memory, disks, failed
services or an inbox. Each gauge is its own command.
_Avoid_: segment, module, block, widget, monitor, sensor

**Reading**:
What a gauge reports at one moment: a level, a summary and a detail.
_Avoid_: status, sample, output, result

**Summary**:
The short form of a reading: a few characters, always the same width for a
given gauge.
_Avoid_: text, label, value

**Detail**:
The long form of a reading: what was measured, over what window, and how fresh
it is.
_Avoid_: tooltip, description

### Levels

**Level**:
How much a reading asks of you, on one scale shared by every gauge.
_Avoid_: severity, class, state, status

**Quiet**:
The level of a reading with nothing to see: no traffic, an empty inbox,
nothing failed.

**Normal**:
The level of a reading with something to see and nothing to do.
_Avoid_: ok

**Attention**:
The level of a reading you should deal with, but not urgently.
_Avoid_: warning, warn

**Critical**:
The level of a reading you should deal with now.
_Avoid_: crit, error, alarm

**Unknown**:
The level of a reading the gauge could not measure. Its detail says why. It is
a reading in its own right, never a missing one.
_Avoid_: error, stale

### Reporting

**Check**:
Taking one reading and stopping.
_Avoid_: once, poll

**Watch**:
Reporting continuously: a new reading whenever it changes.
_Avoid_: follow, stream, poll, subscribe

**Format**:
How readings are written out for whoever consumes them, such as a person at a
terminal or a status bar.
_Avoid_: renderer, output mode, backend

**Floor**:
A level below which readings are written as empty, so a display shows a gauge
only when it matters. Chosen by whoever displays the gauge, never by the gauge.
Unknown readings are never below the floor.
_Avoid_: alarm, threshold, hide
