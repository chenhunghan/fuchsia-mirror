# client integration test for structured configuration

This test ensures that each client language of structured configuration is
receiving structured configuration capabilities correctly.

A puppet is defined in each language-specific directory and they all share the
same configuration interface. Each "config receiver" component is launched
by the integration test and the config it returns is checked against expected
values.
