# Bazel Migration Review Result Template

This is the template for the Bazel Migration review result updated to the
reviewed CL as a comment.

```
Review Result Summary:
<review_summary>

Failed Test:
<list_of_titles_of_failed_tests>

Next Step:
<follow_up_actions>

Checklist:
<check_1_title>
    * <result_of_check_1_item_1>, <check_1_item_1_title>
        * <check1_item1_check_result_summary>
    * <result_of_check_1_item_2>, <check_1_item_2_title>
        * <check1_item2_check_result_summary>
    ...
<check_2_title>
    * <result_of_check_2_item_2>, <check_2_item_2_title>
        * <check2_item1_check_result_summary>
    * <result_of_check_2_item_2>, <check_2_item_2_title>
        * <check2_item2_check_result_summary>
    ...
...
```

Note:

1.  If the CL passes all checks and no other actions to follow up, provide
    "LGTM" at "Next Step:" section.
2.  If any test failed, provide the root cause and solution at "Next Step"
    section.
