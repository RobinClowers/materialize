# Copyright Materialize, Inc. and contributors. All rights reserved.
#
# Use of this software is governed by the Business Source License
# included in the LICENSE file at the root of this repository.
#
# As of the Change Date specified in that file, in accordance with
# the Business Source License, use of this software will be governed
# by the Apache License, Version 2.0.

# Test creating and dropping various views and sources that depend upon
# on another, and indices on those views and sources.

$ set schema={
    "type": "record",
    "name": "row",
    "fields": [
      {"name": "a", "type": "long"},
      {"name": "b", "type": "long"}
    ]
  }

$ kafka-create-topic topic=data

> CREATE CONNECTION kafka_conn
  TO KAFKA (BROKER '${testdrive.kafka-addr}');

> CREATE CONNECTION IF NOT EXISTS csr_conn TO CONFLUENT SCHEMA REGISTRY (
    URL '${testdrive.schema-registry-url}'
  );

> CREATE SOURCE data
  FROM KAFKA CONNECTION kafka_conn (TOPIC 'testdrive-data-${testdrive.seed}')
  FORMAT AVRO USING SCHEMA '${schema}'

> SELECT * FROM data

> CREATE VIEW data_view as SELECT * from data

> SELECT * FROM data_view

> CREATE MATERIALIZED VIEW test1 AS
  SELECT b, sum(a) FROM data GROUP BY b

> SHOW VIEWS
name
---------
data_view

> SHOW MATERIALIZED VIEWS
name    cluster
---------------
test1   default

> SELECT * FROM test1
b  sum
------

$ kafka-ingest format=avro topic=data schema=${schema} timestamp=1
{"a": 1, "b": 1}
{"a": 2, "b": 1}
{"a": 3, "b": 1}
{"a": 1, "b": 2}

> SELECT * FROM test1
b  sum
------
1  6
2  1

> SHOW COLUMNS FROM test1
name nullable type
-------------------
b     false   bigint
sum   false   numeric

> SHOW VIEWS LIKE '%data%'
data_view

> SHOW CREATE MATERIALIZED VIEW test1
name                      create_sql
-----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------
materialize.public.test1  "CREATE MATERIALIZED VIEW \"materialize\".\"public\".\"test1\" IN CLUSTER \"default\" AS SELECT \"b\", \"pg_catalog\".\"sum\"(\"a\") FROM \"materialize\".\"public\".\"data\" GROUP BY \"b\""

# Materialized view can be built on a not-materialized view.
> CREATE MATERIALIZED VIEW test2 AS
  SELECT b, 1 + sum(a + 1) FROM data_view GROUP BY b

> SELECT * FROM test2
b  ?column?
-----------
1  10
2  3

# Materialize data_view.
> CREATE DEFAULT INDEX ON data_view

> SELECT * FROM data_view
a  b
----
1  1
2  1
3  1
1  2

> CREATE VIEW test3 AS
  SELECT b, min(a) FROM data_view GROUP BY b

> SELECT * FROM test3
b  min
------
1  1
2  1

> CREATE MATERIALIZED VIEW test4 AS
  SELECT b, max(a) FROM data_view GROUP BY b

> SELECT * FROM test4
b  max
------
1  3
2  1

# Unmaterialize data view.
> DROP INDEX data_view_primary_idx

# Can continue to select from view that depends on the unmaterialized view.
> SELECT * FROM test4
b  max
------
1  3
2  1

> SELECT * FROM test4 where b = 2
b  max
------
2  1

> SELECT * from data_view
a  b
----
1  1
2  1
3  1
1  2

# Cannot create sink from unmaterialized view.
! CREATE SINK not_mat_sink2 FROM data_view
  INTO KAFKA CONNECTION kafka_conn (TOPIC 'testdrive-data-view2-sink-${testdrive.seed}')
  FORMAT AVRO USING CONFLUENT SCHEMA REGISTRY CONNECTION csr_conn
  ENVELOPE DEBEZIUM
contains:data_view is a view, which cannot be exported as a sink

# Can create indexed view from unmaterialized view.
> CREATE VIEW test5 AS
  SELECT b, max(a) AS c FROM data_view GROUP BY b
> CREATE DEFAULT INDEX ON test5

# or from an indexed unmaterialized view
! CREATE SINK not_mat_sink2 FROM test5
  INTO KAFKA CONNECTION kafka_conn (TOPIC 'testdrive-data-view2-sink-${testdrive.seed}')
  FORMAT AVRO USING CONFLUENT SCHEMA REGISTRY CONNECTION csr_conn
  ENVELOPE DEBEZIUM
contains:test5 is a view, which cannot be exported as a sink

$ set-regex match=(\s\(u\d+\)|\n) replacement=

> EXPLAIN SELECT * FROM test5;
"Explained Query (fast path):  ReadExistingIndex materialize.public.test5_primary_idxUsed Indexes:  - materialize.public.test5_primary_idx"

> SELECT * FROM test5
b  c
------
1  3
2  1

> SELECT c+b from test5
4
3

> CREATE INDEX idx1 ON test5(c)

! SELECT * FROM idx1
contains:catalog item 'materialize.public.idx1' is an index and so cannot be depended upon

# If there exists a second primary index, dropping one primary index will not
# unmaterialize the view.
> DROP INDEX test5_primary_idx

> EXPLAIN SELECT * FROM test5
"Explained Query (fast path):  Project (#1, #0)    ReadExistingIndex materialize.public.idx1Used Indexes:  - materialize.public.idx1"

> SELECT * from test5
b  c
------
1  3
2  1

> SELECT c-b from test5
2
-1

# Unmaterialize test5.
> DROP INDEX idx1

# Still works.
> SELECT * from test5
b  c
------
1  3
2  1

# Test that materialized views can be even if it requires multiple layers of
# recursing through the AST to find a source.
> CREATE MATERIALIZED VIEW test6 AS SELECT (-c + 2*b) AS d FROM test5

> SELECT * from test6
d
----
-1
3

# Dependencies are still queryable after creating a dependent
# materialized view.
> SELECT * from test5
b  c
------
1  3
2  1

> SELECT * from data_view
a  b
----
1  1
2  1
3  1
1  2

# Rematerialize data_view creating an index on it.
> CREATE INDEX data_view_idx on data_view(a)

> SELECT * from data_view
a b
---
1 1
2 1
3 1
1 2

# Existing materialized dependencies can be selected from as normal.
> SELECT * from test6
d
----
-1
3

# Dependencies can be selected from again if they do not depend on any other raw
# source.
> SELECT * from test5
b  c
------
1  3
2  1

# Create a suboptimal second index on the same column in data_view.
> CREATE INDEX data_view_idx2 on data_view(a)

> SELECT * from data_view
a b
---
1 1
2 1
3 1
1 2

> SELECT * from test6
d
----
-1
3

> SELECT * from test5
b  c
------
1  3
2  1

# Delete the first copy of the same index and ensure everything selects as
# normal.
> DROP INDEX data_view_idx

> SELECT * from data_view
a b
---
1 1
2 1
3 1
1 2

> SELECT * from test6
d
----
-1
3

> SELECT * from test5
b  c
------
1  3
2  1

# Materialized sources tests

$ kafka-create-topic topic=mat

> CREATE SOURCE mat_data
  FROM KAFKA CONNECTION kafka_conn (TOPIC 'testdrive-mat-${testdrive.seed}')
  FORMAT AVRO USING SCHEMA '${schema}'

> CREATE DEFAULT INDEX ON mat_data

> SELECT * from mat_data

$ kafka-ingest format=avro topic=mat schema=${schema} timestamp=1
{"a": -1, "b": 0}
{"a": -1, "b": 1}
{"a": 3, "b": 4}
{"a": 1, "b": 2}

> SELECT * from mat_data
a  b
----
-1 0
-1 1
3  4
1  2

> SHOW SOURCES
name     type    size
---------------------
data              kafka     ${arg.default-storage-size}
data_progress     progress  <null>
mat_data          kafka     ${arg.default-storage-size}
mat_data_progress progress  <null>

# If there exists another index, dropping the primary index will not #
# unmaterialize the source. This also tests creating a default index when the
# default index name is already taken.

> CREATE DEFAULT INDEX ON mat_data

> DROP INDEX mat_data_primary_idx

> SELECT a+b from mat_data
-1
0
7
3

# Can create both materialized and unmaterialized views from materialized
# source.
> CREATE MATERIALIZED VIEW test7 as SELECT count(*) from mat_data

> SELECT * from test7
count
-----
4

> CREATE VIEW test8 as SELECT -b as c, -a as d from mat_data

> SELECT * from test8
c  d
-----
0  1
-1 1
-4 -3
-2 -1

# Unmaterialize source.
> DROP INDEX mat_data_primary_idx1

# Still works.
> SELECT * from mat_data
a  b
----
-1 0
-1 1
3  4
1  2

> SELECT * from test7
count
-----
4

> SELECT * from test8
c  d
-----
0  1
-1 1
-4 -3
-2 -1

$ kafka-ingest format=avro topic=mat schema=${schema} timestamp=2
{"a": -3, "b": 0}
{"a": -1, "b": 0}
{"a": 0, "b": 4}
{"a": 1, "b": 2}

# Rematerialize source.
> CREATE INDEX mat_data_idx3 on mat_data(b)

> SELECT * from mat_data
a  b
----
-1 0
-1 1
3  4
1  2
-3 0
-1 0
0  4
1  2

> SELECT * from test7
count
-----
8

> SELECT * from test8
c    d
------
0    1
-1   1
-4  -3
-2  -1
0    3
0    1
-4   0
-2  -1
