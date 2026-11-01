use std::time::Duration;

/// The largest frame either side may send, counted after the length field. It
/// is also the largest single cell a result can carry: a plugin holding a larger
/// value answers with an error naming the column, never a truncated value.
///
/// ponytail: one value per frame caps a cell at 64 MiB; chunked large values
/// lift it when a plugin needs them.
pub const MAX_FRAME: u32 = 64 << 20;

/// A query's data credit, in data-frame payload bytes. A plugin may send a data
/// frame that fits in what is left, or any one frame when nothing is
/// outstanding, so a single row larger than the window still gets through alone.
pub const WINDOW: u64 = 8 << 20;

/// When a plugin should close a batch, whichever comes first: so first rows show
/// quickly. A guideline for plugins; the host enforces only the credit.
pub const BATCH_TARGET: usize = 1 << 20;
pub const BATCH_ROWS: usize = 1024;

/// The most cells one record batch may carry, counting a batch with no
/// columns as one cell a row: a rule, enforced on both sides. A cell costs
/// the host to render whether or not it took bytes on the wire, and a Null
/// column takes none, so the frame's size alone cannot bound it. A plugin
/// sends at most `MAX_BATCH_CELLS / columns` rows a batch.
pub const MAX_BATCH_CELLS: usize = 1 << 20;
pub const BATCH_AGE: Duration = Duration::from_millis(100);

/// From `cancel` sent to the query's terminal frame received. Past it the host
/// kills the plugin.
pub const CANCEL_DEADLINE: Duration = Duration::from_secs(5);

pub const HELLO_DEADLINE: Duration = Duration::from_secs(10);

/// Vendor logins are slow; the ODBC bridge also sets the driver's own login
/// timeout below this.
pub const OPEN_DEADLINE: Duration = Duration::from_secs(60);

/// From `close` sent to the plugin killed if it has not exited.
pub const CLOSE_GRACE: Duration = Duration::from_secs(2);

/// How much of a plugin's stderr the host keeps, for the error when it dies.
pub const STDERR_RING: usize = 64 << 10;
