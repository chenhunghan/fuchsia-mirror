# Migration Package Selection

## Find a package

Use the "gsheets" skill: In the **control table**, find the first row from the top where the "Migration Status" column is "ready_for_migration" and the "Owner" column is empty.
    * If no row meets the requirements, fail the Migration Package Selection step with the reason: "No migration package is found."
    * The row meets the requirements is the **migration package**.


## Claim migration ownership

For the **migration package**, update the "Owner" column to the current user to claim ownership.

Wait for 5 seconds and then re-check the row whether the "Owner" column is the current user.
    * If not, refer to [Find a package](#find-a-package) to find another package for the migration.


## Update Control Table

After the ownership of the **migration package** is claimed, change "Migration Status" to "in-progress".