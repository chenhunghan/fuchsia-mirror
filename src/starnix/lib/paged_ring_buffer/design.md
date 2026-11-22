# Lockless Ring Buffer Design

This is the design of the ring buffer behind Starnix `tracefs`. Any number of
threads append trace records to it without taking a lock, and a trace reader
takes them a page at a time in the Linux ftrace page format. Use it to build
an implementation, and to review one against it.

## Context

A ring buffer is a fixed-size FIFO whose slots form a circle. Writers add data
at the tail, a reader removes the oldest data at the head, and both wrap
around. When the buffer fills, it either rejects new data or overwrites the
oldest.

In Starnix, writers are Linux programs writing to `tracefs`, and each write
becomes one record. A trace consumer reads one buffer per CPU through
`per_cpu/cpuN/trace_pipe_raw` and parses the pages as ftrace ring-buffer
pages.

The Linux algorithm doesn't carry over, because it relies on how Linux writers
behave. A Linux buffer belongs to one CPU, only code on that CPU writes to it,
and writers nest: an interrupt that writes a record finishes before the code
it interrupted continues.

Starnix writers are ordinary threads, and any thread's write can go to any
buffer. The scheduler can pause a writer at any instruction for any length of
time, while other writers fill the buffer and wrap it many times. Most of this
design is about what a writer finds when it resumes.

The write and read paths take no lock, because a lock shared by all writers
would let one paused writer stall the rest.

A paused writer can still delay the reader. A page becomes readable only once
every writer that reserved space on it has committed, so until the paused
writer resumes, the reader gets Nothing ready for that page and every page
after it. Meanwhile the other writers keep filling the ring. Once it wraps onto
the paused writer's page, they fail with Full (overwrite off) or drop their
records (overwrite on).

## Usage

Any number of writers append records at once, and one reader at a time takes
finished pages. A controller creates the buffer with a capacity and an
overflow policy, and enables and disables it. Its calls are rare and take a
lock that writers and the reader never touch. Disabling drops the contents
and frees the memory; enabling starts again from empty.

A writer reserves space for a record of a given size, writes the record into
that space, and commits it. Between reserve and commit, the space belongs only
to that writer, so it can write the record in place without a lock while other
writers reserve their own space.

The reader asks for one page and gets a copy of the oldest finished page, or
an answer that none is ready.

| Operation | Outcomes                                                         |
| --------- | ---------------------------------------------------------------- |
| reserve   | Reserved, with the record's time delta; Full (no space,          |
|           | overwrite off); Dropped (overwrite on, but the record can't be   |
|           | placed without waiting); Disabled; Invalid size.                 |
| commit    | Always succeeds.                                                 |
| read      | A page; Nothing ready; Disabled; Busy (another read is running); |
|           | Buffer too small.                                                |
| enable    | Enabled (no change if already enabled); Out of memory, leaving   |
|           | the buffer disabled.                                             |
| disable   | Disabled, with the counts of lost records and lost pages.        |

A record must fit in one page's record area. A writer must commit every
reservation promptly and with a complete record, because disabling the buffer
waits for every outstanding reservation.

## Data model

### Pages

The buffer holds a fixed number of pages, each the system page size, in the
ftrace layout, little-endian:

```
 bytes 0..8    timestamp   time of the first record on the page, in ns
 bytes 8..16   commit      low bits: number of record bytes that follow
                           bit 31:   records were lost before this page
 bytes 16..    records     packed records, then unused space
```

A consumer parses `commit` bytes of records after the header and ignores the
rest of the page.

### Records

Each record starts with a 32-bit header holding a 5-bit type-or-length field
and a 27-bit time delta, the time since the previous record on the page. The
first field, `type_len`, selects the record type:

| `type_len` | Type          | Meaning                                         |
| ---------- | ------------- | ----------------------------------------------- |
| 1 to 28    | Data          | Payload of `type_len * 4` bytes.                |
| 0          | Data          | Payload length in the first u32 of the payload, |
|            |               | for payloads over 112 bytes.                    |
| 29         | Padding       | Skipped by consumers. With `time_delta` 0, it   |
|            |               | fills the rest of the page; otherwise its       |
|            |               | length is in the next u32.                      |
| 30         | `TIME_EXTEND` | The next u32 holds bits 27 to 58 of the delta   |
|            |               | to the following record.                        |
| 31         | `TIME_STAMP`  | An absolute time: `time_delta` holds bits 0 to  |
|            |               | 26 and the next u32 holds bits 27 to 58.        |

The caller writes data records, with the time delta the buffer gives it. The
buffer writes a time-extend record in front of any record whose delta doesn't
fit in 27 bits, about 134 ms. It never writes padding or time-stamp records:
the space after the last record lies outside `commit`, and a page's first
record takes its absolute time from the page header.

### Overflow policy

When every page holds unread data, a buffer created with overwrite on lets a
writer that needs space drop the oldest finished page, and counts it as a
lost page. With overwrite off, that writer fails with Full and the old data
stays. Capacity is fixed between enable and disable.

### Shared state

The pages sit in a fixed circular order. A page holds only the ftrace header
and records, because consumers parse its bytes directly. The buffer keeps the
state in its own metadata memory, outside the page data:

| State            | Contents                                         |
| ---------------- | ------------------------------------------------ |
| tail word        | Index of the page writers append to, and a       |
|                  | generation that increases on every move.         |
| head word        | Index of the oldest page, a generation, and an   |
|                  | owner bit held by the thread consuming the head. |
| claim word       | Offset of the next free byte, a closed bit, and  |
| (per page)       | the low bits of the last claimed record's time.  |
| writer state     | An open bit, a closing bit, and the number of    |
| (per page)       | active writers on the page.                      |
| ring word        | An enabled bit and the number of reservations    |
|                  | and reads in progress. A reservation is in       |
|                  | progress until it is committed.                  |
| last time        | The latest record time handed out.               |
| lost flag        | Set when records or pages were lost since the    |
|                  | last read.                                       |
| reader flag      | Set while a read is running.                     |

The pages from the head up to the tail hold data, and the pages after the tail
and before the head are reset and free. The buffer is full when the page after
the tail is the head, and empty when the head is the tail and no space has been
claimed on it.

Resetting a page sets its claim word to the start of the record area, open,
with no time, and then sets its writer state to open=1, closing=0, and active writers=0.

## Guarantees

Integrity:

1. Two reservations never overlap, and every reservation lies inside one page's
   record area.
2. The reader never sees a page while any reservation on it is still being
   written, and it sees every byte that a committed record wrote.
3. A page's `commit` field equals the bytes of the records on it, packed from
   the start of the record area.
4. Nothing from a page's earlier use appears inside the `commit` range of a
   later use.

Delivery and loss:

5. Pages reach the reader in the order they were finished, each at most once.
6. Every committed record is delivered, dropped on a counted lost page, still
   in the buffer, or dropped by disable. A record that was never placed is
   counted or reported to its writer.
7. With overwrite off, no unread page is dropped. With overwrite on, a writer
   never fails with Full.
8. A delivered page that follows lost data has its lost-records bit set.

Time:

9. Within a page, record times never decrease, and adding the deltas from the
   page header gives each record's time exactly.
10. A page's first record is no earlier than the last record of the page before
    it.

Lifecycle:

11. Once disable starts, no reserve or read succeeds until the next enable
    finishes. Disable frees memory only after every earlier reservation and read
    has finished.
12. Enabling an enabled buffer changes nothing.

Progress:

13. No writer or reader waits for another thread to be scheduled. A writer that
    can't place its record without waiting drops it and counts it. A reader
    that finds the head busy reports Nothing ready.
14. Writing and reading never allocate or free memory.

## Concurrency

### Sharing a page among writers

Several writers reserve space on the same page at once, and each needs its own
range. A writer reads the page's claim word, works out the record's time and
the length it needs (see Stamping records), and checks that the page is open
and the record fits. It then compare-and-swaps (CAS) the claim word from the
value it read to the new offset and time. If another writer got there first,
the CAS fails and the writer starts over from the value it returned. A failed
claim takes no space, and the offset never passes the end of the page.

### Knowing when a page is complete

Writers finish in any order. A slow writer can still be copying its record into
a page after faster writers have filled it and moved several pages on. So the
claimed offset doesn't tell the reader that a page is done.

Instead, each page counts its active writers. A writer joins the page
(increments the count) before it claims space, and leaves it (decrements the
count) with release ordering after it writes its record. The reader takes a
page only when an acquire read of its writer state shows the page closed with
no active writers. Every change to the writer state is a read-modify-write, so
the value the reader sees is in the release sequence of every writer's
decrement, and every record write happens before the reader's copy.

### Closing a page

The page's size has to be fixed when it closes, while writers may be claiming
space on it at that moment. A writer whose record doesn't fit closes the page
before it leaves it, and the reader closes the tail page when the head has
caught up to an idle, non-empty tail.

Only one thread closes a page and writes its `commit` field. A thread that
closes the page first sets the closing bit in the writer state with an atomic
OR. The OR is a single atomic step, so when several threads close the page at
once, exactly one finds the bit clear. The others find it set and do nothing
more.

The winner then sets the closed bit in the claim word with another atomic OR,
which also returns the final offset. Claims before that OR are inside the
offset, and claims after it see the closed bit and fail. The winner writes
`commit` and then clears the open bit in the writer state with release ordering.
That stops new writers from joining and makes `commit` visible to the reader
along with the records. Writers that claimed space before the close may still be
writing, which is why the reader also waits for the active-writer count to reach
zero.

### Reading a page and reusing it

The reader copies the head page out while the page stays in the ring, rather
than swapping it out as Linux does (see Design tradeoffs). It takes the head by
setting the owner bit in the head word with a CAS, then checks that the page is
complete and isn't the tail. It copies the page into the caller's buffer,
resets the page, and stores the head word with the next index, the next
generation, and the owner bit clear, with release ordering. While the reader
owns the head, no writer can drop that page or move the tail onto it.

When the head and tail are on the same page, the reader closes the page only
if it is idle and non-empty. If no space has been claimed on the page or
writers are active on it, and the tail word is unchanged since the head was
read, so the two were equal at the same moment, the reader leaves the page open
and reports Nothing ready. Otherwise, if the page is still open, the reader
joins it and checks that the tail word has not moved and that space has been
claimed. If both hold, it closes the page and leaves it. If no space has been
claimed, it leaves the page open and reports Nothing ready. Once the page is
closed, whether by the reader or by a writer that was paused before it moved
the tail, the reader moves the tail onto the next page instead of waiting for
that writer. If the next page is the head and an overwriting writer owns it,
the reader leaves the tail to that writer and reports Nothing ready. Whenever
the tail word has moved, the reader starts over. The reader leaves the page
before it takes the head, because the head page is complete only when no
writers are active on it, and the reader counts as one while it has joined.

A reused page must not be touched by writers from its earlier use, and writers
in its new use must not see the old closed state. The first holds because the
page is reset only after its active writers have left. The second holds because
the reset ends with a release store that opens the writer state, and a writer
joins with an acquire read-modify-write on that state, so it sees the reset
claim word. A writer moving the tail onto a free page has also acquired the
head word, which was stored after that page's reset.

### Stale threads (ABA)

A reader or writer that reads shared state, is paused, and resumes after the
ring has wrapped can find a page that looks the same but now belongs to a newer
use. If it acts on what it remembers, it can write into, close, or drop that
page.

The head and tail words carry generations, and every update to them is a CAS
from the exact value the thread read, so a stale update fails. Each index
shares a 64-bit word with its generation. With at most 2^20 pages, the
generation gets 43 bits, which take about 97 days to wrap at a million moves a
second.

Pages are protected by their active-writer count. A writer joins the tail page
and then reads the tail word again; if it changed, the writer leaves and starts
over. A page with active writers is never reset, so a writer that passes this
check claims space on, and closes, only the page it meant to. A writer that
can't join the tail page because it is closed, or whose claim fails because the
page is closed, moves the tail and starts over.

### Moving the head

The reader moves the head when it takes a page. With overwrite on, a writer
that needs space also moves it, by dropping the oldest page. Only the holder
of the owner bit may move the head, so a page is never both delivered and
dropped.

A writer decides the buffer is full only if the tail word is unchanged after it
reads the head word. Every tail move changes the generation, so the tail did
not move between the two reads, and the head and tail it saw were current
together when it read the head.

An overwriting writer takes the owner bit with an acquire CAS from the exact
head word it read. It then checks that the head page is complete and that the
tail word still holds the value that showed the buffer full. If the page still
has active writers, the overwriter restores the head word, drops its own
record, counts it, and sets the lost flag. If the tail moved, it restores the
head word and starts over. Otherwise it resets the page, sets the lost flag,
and stores the next head word with release ordering. The page is now free, and
the writer starts over, which moves the tail onto it.

Each owner CAS reads the previous owner's release store, so every mover sees
the pages as the last one left them. A writer that finds the owner bit already
set drops its record, and the reader reports Nothing ready.

When the reader delivers a page, it clears the lost flag. If the flag was set,
it sets the lost-records bit in its copy of the page header.

### Stamping records

The reader rebuilds record times by adding deltas in page order. If a writer
read the clock and then claimed its space, a pause between the two could give
its record an earlier time than the record before it. Every time after it on
the page would then decode wrong, so a record's time and position are decided
in the same CAS.

The claim word holds the low bits of the last claimed record's time, about 46
bits, a range of about 19.5 hours. On each claim attempt, a writer reads the
clock and the claim word, then:

1. If the claim starts at the beginning of the record area, the record is the
   first on the page. Its time is the later of the clock and the buffer's last
   time. It goes in the page header, the delta is 0, and the claim length is
   the record's size.
2. Otherwise, the writer recovers the previous record's full time from the low
   bits and its clock. The record's time is the later of the clock and that
   time, and the delta is the difference. If the delta fits in 27 bits, the
   claim length is the record's size. If not, the claim adds 8 bytes for a
   time-extend record in front of it, and the record's own `time_delta` holds
   the low 27 bits.
3. The low bits identify the previous time only while the two times are less
   than half the range apart. If the buffer's last time is more than about 4.9
   hours behind the clock, the writer closes the page instead, and the record
   becomes the first on the next page.

A failed CAS means another record was claimed in between, so the writer reads
the clock again and redoes the steps. After a successful claim it writes the
time-extend record and the page timestamp, if it has them, and raises the
buffer's last time to its record's time. A first record never needs a
time-extend record, so any record that fits in the record area fits on an
empty page.

### Enabling and disabling

Disabling stops new reservations and reads, waits for the ones in progress to
finish, and then frees the memory, so no reader or writer is still using it.
Every reserve and read enters the buffer with an acquire CAS on the ring word
that fails when the enabled bit is clear, and leaves with a release decrement.
A reservation stays inside until it is committed. Disable clears the enabled
bit and frees the memory only after an acquire read of the ring word shows no
reservations or reads in progress, so every access happens before the free.
Enable resets every page, puts the head and tail on the same page, and sets the
enabled bit with a release store, so every later call sees the reset.

### Never waiting on another thread

Guarantee 13 says no writer or reader waits for another thread to be scheduled,
because that thread may be paused for any length of time. Every step that
several threads may want to take is one of two kinds.

Some steps any thread can finish for the others. Moving the tail onto a free
page is one: every writer that finds the tail page closed CASes the tail word
from the value it read. One CAS wins, and the rest fail, see the new tail, and
continue on it. The reader does the same when the head has caught up to a
closed tail page. Neither writers nor the reader depend on the thread that
closed the page to move the tail.

Other steps belong to the one thread that won the right to take them: closing a
page (won by the OR on the closing bit) and consuming the head (won by the owner
bit). Nobody waits for the winner to finish. Writers that find the page closed
move the tail and claim space on the next page, writers that find the head owned
drop their records, and the reader reports Nothing ready.

A paused writer still delays delivery of its page and the pages after it (see
Context), and while a paused reader owns the head, overwriting writers drop
their records instead of dropping that page.

## Design tradeoffs

Claims use a CAS rather than the atomic add that Linux uses. An add never
retries, so it is cheaper when many writers hit one page, but it can't make the
claim length depend on the value it replaces. Here a record needs 8 more bytes
when its delta needs a time-extend record, and the delta depends on the
previous record's time. A CAS only fails when another writer succeeded, so
writers still make progress.

The reader closes the tail page only when the head has caught up to it and the
reader finds no writer active on it. While closed pages wait ahead of the tail,
or while it sees writers active on the tail page, the reader leaves the tail
alone. Once writers go quiet, a read delivers the trailing records on a partly
filled page instead of leaving them unread until a later write fills the page
or disable drops them.

The closer is picked by a separate closing bit in the writer state, not by the
OR on the claim word's closed bit. Every closer, writer or reader, has joined
the page, so the claim word's OR alone could pick the winner: a losing OR finds
the bit already set and changes nothing. The closing bit costs one more atomic
operation per closed page.

The reader copies the head page while it stays in the ring. Linux instead swaps
a spare page into the ring for the head, so writers can never reach the page
being copied. Copying without the swap needs no spare page and no links between
pages, and keeps the pages in a fixed order. While the copy runs, though, an
overwriting writer that needs that page drops its record.

After hours with no records, a writer starts a new page rather than writing a
time-stamp record. That wastes the rest of a page, but only after a long gap,
and avoids a record type whose layout changed in Linux 4.17.

A writer that finds a paused writer still active on the head page drops its
record rather than wait. Waiting would turn that writer's count into a lock
held across a preemption, stalling every writer behind it. Records are lost
when the ring wraps onto a paused writer, and they are counted.

Every reservation must be committed with a complete record. Abandoning one
would mean turning it into a padding record, which needs a nonzero delta and so
shifts the times around it.
