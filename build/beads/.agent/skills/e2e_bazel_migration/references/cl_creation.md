# CL Creation

## Commit Changes

1. Refer to [Commit Message Template](../assets/commit_message_template.md) to create the commit message for the changes.

2. Run `git commit` command to commit the changes using the generated commit message.


## Upload to Gerrit

* Run `git push origin HEAD:refs/for/main -o hashtag=bazel-migration` to upload the changes to Fuchsia Gerrit and create a CL.


## Update Control Table

1.  Use "gsheets" skill: In the **control table**, update the "Migration Status" column for the **migration package** to "CL_created".

2.  Wait for 5 seconds and then re-check the row to verify that the update was successful.