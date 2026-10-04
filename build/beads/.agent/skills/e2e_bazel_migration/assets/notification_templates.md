# Notification Templates

## Start Notification Template

**Notification Subject:**
`Bazel Migration for <migrating_target_name> started`

**Notification Body:**
```
The migration for <migrating_target_name> started at <current_time>.
Migrated Target: <path_of_the_migrating_target>
Buganizer Ticket: <Buganizer_ticket_number>
```

## Completed Notification Template

**Notification Subject:**
`Bazel Migration for <migrating_target_name> has been completed`

**Notification Body:**
```
The migration for <migrating_target_name> was completed at <current_time>.
Migrated Target: <path_of_the_migrating_target>
Buganizer Ticket: <Buganizer_ticket_number>
Fuchsia Gerrit CL: <Fuchsia_Gerrit_CL_number>
```

## Interrupted Notification Template

**Notification Subject:**
`Bazel Migration for <migrating_target_name> was interrupted`

**Notification Body:**
```
The migration for <migrating_target_name> was interrupted at <current_time>
because of <reason>.
Migrated Target: <path_of_the_migrating_target>
Buganizer Ticket: <Buganizer_ticket_number>
Control Table: <the_go_link_of_fuchsia-bazel-migration-control-table>
```
