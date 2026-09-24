# Snowflake test fixtures

The `.p8` keys here are throwaway RSA key pairs generated for the offline
unit tests in `src/db/snowflake.rs`. They are not registered to any Snowflake
user or account, protect nothing, and are safe to be public -- a secret
scanner flagging one of them can be dismissed.
