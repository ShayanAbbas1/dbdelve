// Run by the image's entrypoint through mongosh, as the root user, against
// MONGO_INITDB_DATABASE (dbdelve_dev), on the first start of an empty volume.

// Created in dbdelve_dev so the dev URL needs no `authSource`. Without
// listDatabases, `listDatabases` answers with the databases the user holds a
// role on, which is exactly these two.
db.createUser({
  user: "dbdelve",
  pwd: "dbdelve",
  roles: [
    { role: "readWrite", db: "dbdelve_dev" },
    { role: "readWrite", db: "dbdelve_archive" },
  ],
});

// Fixed ids, so the tests and the archive's references can name them.
const accountIds = [1, 2, 3, 4, 5].map((n) =>
  ObjectId(`65a4f1c000000000000000${String(n).padStart(2, "0")}`)
);

// Dijkstra's email is null and Bruce Lee has none at all: a document store
// tells those apart, and a grid that renders both the same loses that.
db.accounts.insertMany([
  {
    _id: accountIds[0],
    external_id: UUID("018f1f6e-7c2a-7000-8000-000000000001"),
    name: "Ada Lovelace",
    email: "ada@example.test",
    plan: "enterprise",
    balance: NumberDecimal("125000.50"),
    active: true,
    tags: ["founder", "priority"],
    metadata: { timezone: "Europe/London", features: { audit: true, seats: 250 } },
    created_at: ISODate("2024-01-15T09:30:00Z"),
  },
  {
    _id: accountIds[1],
    external_id: UUID("018f1f6e-7c2a-7000-8000-000000000002"),
    name: "Grace Hopper",
    email: "grace@example.test",
    plan: "team",
    balance: NumberDecimal("8192.00"),
    active: true,
    tags: ["compiler", "navy"],
    metadata: { timezone: "America/New_York", languages: ["COBOL", "English"] },
    created_at: ISODate("2024-02-29T12:00:00Z"),
  },
  {
    _id: accountIds[2],
    external_id: UUID("018f1f6e-7c2a-7000-8000-000000000003"),
    name: "Edsger Dijkstra",
    email: null,
    plan: "free",
    balance: NumberDecimal("-0.01"),
    active: false,
    tags: [],
    metadata: { note: "Simplicity is prerequisite for reliability." },
    created_at: ISODate("2024-03-10T18:45:12.123Z"),
  },
  {
    _id: accountIds[3],
    external_id: UUID("018f1f6e-7c2a-7000-8000-000000000004"),
    name: "李小龍",
    plan: "team",
    balance: NumberDecimal("42.42"),
    active: true,
    tags: ["unicode", "香港"],
    metadata: { display_name: "李小龍", emoji: "🐉", rtl: "مرحبا" },
    created_at: ISODate("2024-04-01T00:00:00Z"),
  },
  {
    _id: accountIds[4],
    external_id: UUID("018f1f6e-7c2a-7000-8000-000000000005"),
    name: "Quotes 'n' \"Backslashes\" \\\\",
    email: "escaping@example.test",
    plan: "free",
    balance: NumberDecimal("0.00"),
    active: true,
    tags: ["quotes", "backslash"],
    metadata: { query: '{ "$where": "not an operator" }', path: "C:\\demo\\file" },
    created_at: ISODate("2024-05-05T05:05:05Z"),
  },
]);
db.accounts.createIndex({ external_id: 1 }, { unique: true });

// Five thousand documents, so scrolling, sorting and the row-count chip have
// something to work against rather than a grid that fits on screen.
const measurements = [];
for (let sample = 1; sample <= 5000; sample++) {
  measurements.push({
    _id: sample,
    recorded_at: new Date(Date.UTC(2025, 0, 1) + sample * 15000),
    sensor: `sensor-${String(((sample - 1) % 24) + 1).padStart(2, "0")}`,
    // A null every 97th document, so null rendering is reachable by scrolling.
    temperature_c: sample % 97 === 0 ? null : 18.0 + (sample % 150) / 10.0,
    pressure_kpa: NumberDecimal((98 + (sample % 700) / 1000).toFixed(3)),
    healthy: sample % 113 !== 0,
    samples: [sample % 10, sample % 20, sample % 30],
    payload: {
      sequence: NumberLong(sample),
      firmware: `v${1 + (sample % 3)}.${sample % 10}`,
      flags: [sample % 2 === 0, sample % 5 === 0],
    },
  });
}
db.measurements.insertMany(measurements);

// Values far larger than a cell can show, and values a cell would misread:
// embedded newlines and semicolons, a nested null, raw bytes, deep nesting.
let deep = { depth: 30, leaf: "bottom" };
for (let depth = 29; depth >= 1; depth--) deep = { depth, child: deep };
db.documents.insertMany([
  {
    _id: 1,
    title: "Multiline text",
    body: "first line\nsecond line\nthird line; with a semicolon",
    document: { kind: "short", nested: { null_value: null } },
    binary_value: HexData(0, "00010203feff"),
  },
  {
    _id: 2,
    title: "Large values",
    body: "DBDelve keeps the complete value while the grid clips visually. ".repeat(2048),
    document: {
      kind: "large",
      values: Array.from({ length: 500 }, (_, i) => ({ index: i + 1, square: (i + 1) * (i + 1) })),
    },
    binary_value: HexData(0, "deadbeef".repeat(4096)),
  },
  { _id: 3, title: "Deep nesting", document: deep },
]);

// Every BSON type a document can hold, one per field, so each has a rendering
// to get right. `_id` is a string here, unlike the ObjectIds and numbers
// elsewhere: one collection's ids are not another's.
db.bson_types.insertOne({
  _id: "every-type",
  double: 3.14159,
  double_whole: Double(2),
  int32: NumberInt(2147483647),
  int64: NumberLong("9223372036854775807"),
  decimal128: NumberDecimal("1234567890.123456789012345678901234"),
  string: "text",
  empty_string: "",
  boolean: true,
  null: null,
  object_id: ObjectId("65a4f1c0ffffffffffffffff"),
  date: ISODate("2024-01-15T09:30:00.123Z"),
  date_before_epoch: ISODate("1815-12-10T00:00:00Z"),
  timestamp: Timestamp({ t: 1705311000, i: 1 }),
  binary_generic: HexData(0, "00ff"),
  binary_uuid: UUID("018f1f6e-7c2a-7000-8000-0000000000ff"),
  regex: /^dbdelve.*$/i,
  javascript: Code("function () { return 1; }"),
  min_key: MinKey(),
  max_key: MaxKey(),
  array: [1, "two", 3.0, null, { four: 4 }, [5]],
  empty_array: [],
  object: { nested: { deeper: true } },
  empty_object: {},
});

// The same field holding a different type in each document, and fields only
// some documents have: the column set is a property of a sample, never of the
// collection. Field names with a dot or a leading `$` are legal since 5.0 and
// need quoting in any path built from them.
db.mixed_shapes.insertMany([
  { _id: 1, value: "a string" },
  { _id: 2, value: NumberInt(42) },
  { _id: 3, value: NumberLong(42) },
  { _id: 4, value: 42.5 },
  { _id: 5, value: NumberDecimal("42.50") },
  { _id: 6, value: true },
  { _id: 7, value: null },
  { _id: 8 },
  { _id: 9, value: [1, 2, 3] },
  { _id: 10, value: { nested: "object" } },
  { _id: 11, value: ISODate("2024-06-01T00:00:00Z") },
  { _id: 12, value: ObjectId("65a4f1c0000000000000000c"), only_here: "sparse field" },
  { _id: 13, "with.dot": "dotted name", $dollar: "dollar name", "": "empty name" },
]);

// Native GeoJSON, which the SQL engines only have through PostGIS.
db.locations.insertMany([
  { _id: 1, name: "San Francisco", point: { type: "Point", coordinates: [-122.4194, 37.7749] } },
  {
    _id: 2,
    name: "Null Island",
    point: { type: "Point", coordinates: [0, 0] },
    boundary: {
      type: "Polygon",
      coordinates: [[[-1, -1], [1, -1], [1, 1], [-1, 1], [-1, -1]]],
    },
  },
]);
db.locations.createIndex({ point: "2dsphere" });

// References worth following, though nothing enforces them. `orders` keys on a
// compound `_id`, the document spelling of a composite primary key, and
// `order_items` points at that whole subdocument.
db.orders.insertMany([
  { _id: { account_id: accountIds[0], number: 1001 }, placed_at: ISODate("2024-06-01T10:00:00Z"), total: NumberDecimal("4500.00") },
  { _id: { account_id: accountIds[0], number: 1002 }, placed_at: ISODate("2024-06-08T11:30:00Z"), total: NumberDecimal("125.75") },
  { _id: { account_id: accountIds[1], number: 2001 }, placed_at: ISODate("2024-06-12T16:15:00Z"), total: NumberDecimal("890.10") },
]);
db.order_items.insertMany([
  { _id: 1, order: { account_id: accountIds[0], number: 1001 }, description: "Analytical engine time", quantity: 3 },
  { _id: 2, order: { account_id: accountIds[0], number: 1002 }, description: "Punch card stock", quantity: 500 },
  { _id: 3, order: { account_id: accountIds[1], number: 2001 }, description: "Compiler seat", quantity: 1 },
]);

const archive = db.getSiblingDB("dbdelve_archive");
archive.closed_accounts.insertMany([
  { _id: 1, account_id: accountIds[2], closed_at: ISODate("2024-07-01T00:00:00Z") },
  { _id: 2, account_id: accountIds[4], closed_at: ISODate("2024-07-04T12:00:00Z") },
]);

db.createView("account_overview", "accounts", [
  {
    $group: {
      _id: "$plan",
      accounts: { $sum: 1 },
      total_balance: { $sum: "$balance" },
      active_accounts: { $sum: { $cond: ["$active", 1, 0] } },
    },
  },
  { $sort: { _id: 1 } },
]);

// A time-series collection lists beside the others but is backed by a hidden
// `system.buckets.sensor_readings`, which the explorer must not show.
db.createCollection("sensor_readings", {
  timeseries: { timeField: "recorded_at", metaField: "sensor", granularity: "minutes" },
});
db.sensor_readings.insertMany(
  Array.from({ length: 100 }, (_, i) => ({
    recorded_at: new Date(Date.UTC(2025, 0, 1) + i * 60000),
    sensor: { id: `sensor-${(i % 4) + 1}` },
    temperature_c: 18 + (i % 30) / 10,
  }))
);

// A million documents and two hundred fields: the fixtures the grid's paging
// and horizontal scrolling are measured against.
const kinds = ["click", "view", "purchase", "refund", "signup", "churn"];
const eventsStart = Date.UTC(2024, 0, 1);
for (let batch = 0; batch < 100; batch++) {
  const events = [];
  for (let n = batch * 10000 + 1; n <= (batch + 1) * 10000; n++) {
    events.push({
      _id: NumberLong(n),
      account_id: accountIds[n % 5],
      kind: kinds[n % 6],
      amount: NumberDecimal(((n % 100000) / 100).toFixed(2)),
      occurred_at: new Date(eventsStart + n * 1000),
    });
  }
  db.events.insertMany(events, { ordered: false });
}
db.events.createIndex({ account_id: 1, occurred_at: -1 });

// Last on purpose: the compose healthcheck counts this collection, so a
// complete one means the whole seed ran.
db.wide_metrics.insertMany(
  Array.from({ length: 25 }, (_, row) => {
    const i = row + 1;
    const doc = { _id: i };
    for (let c = 1; c <= 199; c++) {
      const name = `c${String(c).padStart(3, "0")}`;
      doc[name] = c % 3 === 1 ? NumberLong(i * c + 1) : c % 3 === 2 ? i * 0.5 + c : `${name}-${i}`;
    }
    return doc;
  })
);
