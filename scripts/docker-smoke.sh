#!/usr/bin/env bash
set -euo pipefail

project="mydb-smoke"
smoke_host="${MYDB_SMOKE_HOST:-127.0.0.1}"
export MYDB_HOST_PORT=13316
export MYDB_HOST_HTTP_PORT=14316
export MYDB_ROOT_PASSWORD=root
export MYDB_ADMIN_PASSWORD=root
export MYDB_ENFORCE_STRONG_PASSWORDS=false

cleanup() {
  if [ "${KEEP:-0}" != "1" ]; then
    docker compose -p "$project" down -v --remove-orphans >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT

docker info >/dev/null
if [ "${NO_BUILD:-0}" = "1" ]; then
  docker compose -p "$project" up -d --no-build
else
  docker compose -p "$project" up -d --build
fi

deadline=$((SECONDS + 120))
until [ "$(docker inspect --format '{{.State.Health.Status}}' mydb 2>/dev/null || true)" = "healthy" ]; do
  if [ "$SECONDS" -ge "$deadline" ]; then
    echo "MyDB container did not become healthy" >&2
    exit 1
  fi
  sleep 2
done
docker exec mydb mydbdump --help >/dev/null
docker exec mydb mydb-cli --admin-password root agent health | grep -q '"healthy": true'

docker run --rm --network "${project}_default" mysql:8.0 \
  mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root \
  --default-character-set=utf8mb4 --execute="
CREATE DATABASE smoke;
USE smoke;
CREATE TABLE players (id BIGINT AUTO_INCREMENT PRIMARY KEY, actor_id VARCHAR(64) UNIQUE, name VARCHAR(64), score BIGINT);
START TRANSACTION;
INSERT INTO players (actor_id,name,score) VALUES ('a1','alice',10),('a2','bob',20);
COMMIT;
INSERT INTO players (actor_id,name,score) VALUES ('a2','bob2',21),('a3','cat',30)
  ON DUPLICATE KEY UPDATE name=VALUES(name),score=VALUES(score);
INSERT IGNORE INTO players (actor_id,name,score) VALUES ('a1','ignored',99);
CREATE INDEX score_idx ON players (score);
CREATE TABLE clans (actor_id VARCHAR(64) PRIMARY KEY, clan_name VARCHAR(64));
INSERT INTO clans VALUES ('a1','red'),('a9','blue');
CREATE TABLE multi_guilds (id BIGINT PRIMARY KEY, name VARCHAR(32) NOT NULL);
INSERT INTO multi_guilds VALUES (1,'red'),(3,'blue');
CREATE TABLE multi_members (actor_id VARCHAR(64) NOT NULL, guild_id BIGINT NOT NULL, rank_id BIGINT NOT NULL, PRIMARY KEY(actor_id,guild_id));
INSERT INTO multi_members VALUES ('a1',1,10),('a2',99,20);
CREATE TABLE multi_ranks (id BIGINT PRIMARY KEY, title VARCHAR(32) NOT NULL);
INSERT INTO multi_ranks VALUES (10,'leader'),(30,'guest');
CREATE TABLE ru_probe (id BIGINT PRIMARY KEY, value BIGINT) ENGINE=InnoDB;
INSERT INTO ru_probe VALUES (1,10);
CREATE TABLE volatile_sessions (id BIGINT PRIMARY KEY, value VARCHAR(64)) ENGINE=MEMORY;
START TRANSACTION;
INSERT INTO volatile_sessions VALUES (1,'survives_rollback');
ROLLBACK;
SELECT COUNT(*) FROM volatile_sessions;
SELECT id,actor_id,name,score FROM players WHERE actor_id IN ('a1','a2','a3') ORDER BY id;
SELECT COUNT(*),SUM(score),AVG(score),MAX(score),MIN(score) FROM players WHERE score >= 10;
SELECT COUNT(*) FROM information_schema.tables WHERE table_schema=DATABASE() AND table_name='players';"
native_count=$(docker exec mydb mydb-cli -h 127.0.0.1 -P 3306 -u root -p root \
  -D smoke -e "SELECT COUNT(*) FROM players")
grep -qx '3' <<<"$native_count"
join_state=$(docker run --rm --network "${project}_default" mysql:8.0 \
  mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root \
  --batch --skip-column-names --execute="USE smoke; SELECT p.actor_id,c.clan_name FROM players p INNER JOIN clans c ON c.actor_id=p.actor_id WHERE c.clan_name='red'; SELECT p.actor_id,c.clan_name FROM players p LEFT OUTER JOIN clans c ON p.actor_id=c.actor_id WHERE p.actor_id IN ('a1','a3') ORDER BY p.id; SELECT p.actor_id,c.actor_id,c.clan_name FROM players p RIGHT JOIN clans c ON p.actor_id=c.actor_id ORDER BY c.actor_id; SELECT COUNT(*) FROM players p CROSS JOIN clans c; SELECT COUNT(*),COUNT(c.clan_name),SUM(p.score) FROM players p LEFT JOIN clans c ON p.actor_id=c.actor_id; SELECT c.clan_name,COUNT(*),SUM(p.score) FROM players p LEFT JOIN clans c ON p.actor_id=c.actor_id GROUP BY c.clan_name ORDER BY c.clan_name; SELECT actor_id FROM players WHERE actor_id IN (SELECT actor_id FROM clans WHERE clan_name='red'); SELECT name FROM players WHERE actor_id=(SELECT actor_id FROM clans WHERE clan_name='red');")
test "$join_state" = $'a1\tred\na1\tred\na3\tNULL\na1\ta1\tred\nNULL\ta9\tblue\n6\n3\t1\t61\nNULL\t2\t51\nred\t1\t10\na1\nalice'
multi_join_state=$(docker run --rm --network "${project}_default" mysql:8.0 \
  mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root \
  --batch --skip-column-names --execute="USE smoke; SELECT p.actor_id,g.name,r.title FROM players p JOIN multi_members m ON m.actor_id=p.actor_id JOIN multi_guilds g ON g.id=m.guild_id JOIN multi_ranks r ON r.id=m.rank_id ORDER BY p.id; SELECT p.actor_id,g.name,r.title FROM players p LEFT JOIN multi_members m ON m.actor_id=p.actor_id LEFT JOIN multi_guilds g ON g.id=m.guild_id LEFT JOIN multi_ranks r ON r.id=m.rank_id ORDER BY p.id; SELECT p.actor_id,m.guild_id,g.id FROM players p JOIN multi_members m ON m.actor_id=p.actor_id RIGHT JOIN multi_guilds g ON g.id=m.guild_id ORDER BY g.id; SELECT COUNT(*) FROM players p CROSS JOIN multi_guilds g JOIN multi_members m ON m.actor_id=p.actor_id AND m.guild_id=g.id; SELECT COUNT(*) FROM players p CROSS JOIN multi_guilds g CROSS JOIN multi_ranks r;")
test "$multi_join_state" = $'a1\tred\tleader\na1\tred\tleader\na2\tNULL\tNULL\na3\tNULL\tNULL\na1\t1\t1\nNULL\tNULL\t3\n1\n12'
advanced_join_state=$(docker run --rm --network "${project}_default" mysql:8.0 \
  mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root \
  --batch --skip-column-names --execute="USE smoke; SELECT * FROM players NATURAL LEFT JOIN multi_members ORDER BY id; SELECT * FROM players NATURAL RIGHT JOIN multi_members ORDER BY actor_id; SELECT p.actor_id,r.id FROM players p JOIN multi_ranks r ON p.score < r.id ORDER BY p.id; SELECT p.actor_id,r.id FROM players p JOIN multi_ranks r ON (p.score < r.id AND p.id <> r.id) OR p.score <=> r.id ORDER BY p.id; SELECT * FROM players JOIN multi_members USING (actor_id) ORDER BY id;")
test "$advanced_join_state" = $'a1\t1\talice\t10\t1\t10\na2\t2\tbob2\t21\t99\t20\na3\t4\tcat\t30\tNULL\tNULL\na1\t1\t10\t1\talice\t10\na2\t99\t20\t2\tbob2\t21\na1\t30\na2\t30\na1\t10\na1\t30\na2\t30\na3\t30\na1\t1\talice\t10\t1\t10\na2\t2\tbob2\t21\t99\t20'
correlated_state=$(docker run --rm --network "${project}_default" mysql:8.0 \
  mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root \
  --batch --skip-column-names --execute="USE smoke; SELECT p.actor_id FROM players p WHERE EXISTS (SELECT 1 FROM multi_members m WHERE m.actor_id=p.actor_id) ORDER BY p.id; SELECT p.actor_id FROM players p WHERE NOT EXISTS (SELECT 1 FROM multi_members m WHERE m.actor_id=p.actor_id) ORDER BY p.id; SELECT p.actor_id FROM players p WHERE p.actor_id IN (SELECT m.actor_id FROM multi_members m WHERE m.guild_id=p.id) ORDER BY p.id; SELECT p.actor_id FROM players p WHERE p.actor_id NOT IN (SELECT m.actor_id FROM multi_members m WHERE m.guild_id=p.id) ORDER BY p.id; SELECT p.actor_id FROM players p WHERE p.score = (SELECT r.id FROM multi_ranks r WHERE r.id=p.score) ORDER BY p.id; SELECT p.actor_id,g.id FROM players p CROSS JOIN multi_guilds g WHERE EXISTS (SELECT 1 FROM multi_members m WHERE m.actor_id=p.actor_id AND m.guild_id=g.id) ORDER BY p.id; SELECT p.actor_id FROM players p WHERE p.score BETWEEN 10 AND 10 AND EXISTS (SELECT 1 FROM multi_members m WHERE m.actor_id=p.actor_id); SELECT p.actor_id FROM players p WHERE (p.score=10 AND EXISTS (SELECT 1 FROM multi_members m WHERE m.actor_id=p.actor_id)) OR p.score=21 ORDER BY p.id;")
test "$correlated_state" = $'a1\na2\na3\na1\na2\na3\na1\na3\na1\t1\na1\na1\na2'
derived_state=$(docker run --rm --network "${project}_default" mysql:8.0 \
  mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root \
  --batch --skip-column-names --execute="USE smoke; SELECT d.actor_id,d.score FROM (SELECT actor_id,score FROM players WHERE score>=20) d WHERE d.score>20 ORDER BY d.actor_id; SELECT d.score,COUNT(*) FROM (SELECT score FROM players) d GROUP BY d.score ORDER BY d.score; SELECT d.actor_id,c.clan_name FROM (SELECT actor_id,score FROM players) d LEFT JOIN clans c ON c.actor_id=d.actor_id ORDER BY d.score; SELECT d.actor_id FROM (SELECT actor_id FROM players) d WHERE EXISTS (SELECT 1 FROM multi_members m WHERE m.actor_id=d.actor_id) ORDER BY d.actor_id;")
test "$derived_state" = $'a2\t21\na3\t30\n10\t1\n21\t1\n30\t1\na1\tred\na2\tNULL\na3\tNULL\na1\na2'
union_state=$(docker run --rm --network "${project}_default" mysql:8.0 \
  mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root \
  --batch --skip-column-names --execute="USE smoke; SELECT actor_id AS k FROM players WHERE id=1 UNION SELECT actor_id FROM players ORDER BY k; SELECT actor_id FROM players UNION ALL SELECT actor_id FROM players ORDER BY actor_id DESC LIMIT 4; SELECT d.actor_id FROM (SELECT actor_id FROM players WHERE id=1 UNION ALL SELECT actor_id FROM players WHERE id=2) d ORDER BY d.actor_id DESC;")
test "$union_state" = $'a1\na2\na3\na3\na3\na2\na2\na2\na1'
set_operation_state=$(docker run --rm --network "${project}_default" mysql:8.0 \
  mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root \
  --batch --skip-column-names --execute="USE smoke; SELECT actor_id FROM players INTERSECT SELECT actor_id FROM clans ORDER BY actor_id; SELECT actor_id FROM players EXCEPT SELECT actor_id FROM clans ORDER BY actor_id; SELECT actor_id FROM players WHERE actor_id='a1' UNION ALL SELECT actor_id FROM players INTERSECT SELECT actor_id FROM multi_members WHERE guild_id=99 ORDER BY actor_id; WITH base AS (SELECT actor_id,score FROM players WHERE score>=20), memberships(actor_id,guild_id) AS (SELECT actor_id,guild_id FROM multi_members) SELECT b.actor_id,m.guild_id FROM base b LEFT JOIN memberships m ON b.actor_id=m.actor_id ORDER BY b.actor_id; SELECT guild_id,rank_id,COUNT(*) FROM multi_members GROUP BY guild_id,rank_id ORDER BY guild_id; WITH RECURSIVE seq(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM seq WHERE n<3) SELECT n FROM seq ORDER BY n;")
test "$set_operation_state" = $'a1\na2\na3\na1\na2\na2\t99\na3\tNULL\n1\t10\t1\n99\t20\t1\n1\n2\n3'
window_state=$(docker run --rm --network "${project}_default" mysql:8.0 \
  mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root \
  --batch --skip-column-names --execute="USE smoke; SELECT actor_id,ROW_NUMBER() OVER (ORDER BY score DESC) AS rn,SUM(score) OVER (ORDER BY id ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) AS running,LAG(score,1,-1) OVER (ORDER BY id) AS prev FROM players ORDER BY id; SELECT rank_id,COUNT(*),SUM(COUNT(*)) OVER (ORDER BY rank_id) FROM multi_members GROUP BY rank_id ORDER BY rank_id;")
test "$window_state" = $'a1\t3\t10\t-1\na2\t2\t31\t10\na3\t1\t61\t21\n10\t1\t1\n20\t1\t2'
join_dml_state=$(docker run --rm --network "${project}_default" mysql:8.0 \
  mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root \
  --batch --skip-column-names --execute="USE smoke; CREATE TABLE join_players (id BIGINT PRIMARY KEY,value BIGINT); CREATE TABLE join_bonuses (id BIGINT PRIMARY KEY,player_id BIGINT,amount BIGINT); CREATE TABLE join_archive (id BIGINT PRIMARY KEY,value BIGINT); INSERT INTO join_players VALUES (1,10),(2,20); INSERT INTO join_bonuses VALUES (1,1,5),(2,2,7); INSERT INTO join_archive SELECT id,value FROM join_players; UPDATE join_players p JOIN join_bonuses b ON p.id=b.player_id SET p.value=p.value+b.amount; DELETE p FROM join_players p JOIN join_bonuses b ON p.id=b.player_id WHERE b.amount=7; SELECT id,value FROM join_players ORDER BY id; SELECT id,value FROM join_archive ORDER BY id;")
test "$join_dml_state" = $'1\t15\n1\t10\n2\t20'
multi_target_dml_state=$(docker run --rm --network "${project}_default" mysql:8.0 \
  mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root \
  --batch --skip-column-names --execute="USE smoke; CREATE TABLE multi_update_accounts (id BIGINT PRIMARY KEY,value BIGINT); CREATE TABLE multi_update_bonuses (id BIGINT PRIMARY KEY,account_id BIGINT,amount BIGINT); INSERT INTO multi_update_accounts VALUES (1,10),(2,20); INSERT INTO multi_update_bonuses VALUES (11,1,5),(12,2,7); UPDATE multi_update_accounts a JOIN multi_update_bonuses b ON a.id=b.account_id SET a.value=a.value+b.amount,b.amount=b.amount+1; SELECT a.id,a.value,b.amount FROM multi_update_accounts a JOIN multi_update_bonuses b ON a.id=b.account_id ORDER BY a.id; START TRANSACTION; DELETE a,b FROM multi_update_accounts a JOIN multi_update_bonuses b ON a.id=b.account_id WHERE a.id=1; SELECT COUNT(*) FROM multi_update_accounts; SELECT COUNT(*) FROM multi_update_bonuses; ROLLBACK; DELETE FROM a,b USING multi_update_accounts a JOIN multi_update_bonuses b ON a.id=b.account_id; SELECT COUNT(*) FROM multi_update_accounts; SELECT COUNT(*) FROM multi_update_bonuses;")
test "$multi_target_dml_state" = $'1\t15\t6\n2\t27\t8\n1\n1\n0\n0'
keyless_join_dml_state=$(docker run --rm --network "${project}_default" mysql:8.0 \
  mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root \
  --batch --skip-column-names --execute="USE smoke; CREATE TABLE keyless_accounts (player_id BIGINT,value BIGINT); CREATE TABLE keyless_bonuses (player_id BIGINT,amount BIGINT); INSERT INTO keyless_accounts VALUES (1,10),(1,10),(2,20); INSERT INTO keyless_bonuses VALUES (1,5),(2,7); START TRANSACTION; UPDATE keyless_accounts a JOIN keyless_bonuses b ON a.player_id=b.player_id SET a.value=a.value+b.amount,b.amount=b.amount+1; SELECT value,COUNT(*) FROM keyless_accounts GROUP BY value ORDER BY value; SELECT SUM(amount) FROM keyless_bonuses; ROLLBACK; DELETE a,b FROM keyless_accounts a JOIN keyless_bonuses b ON a.player_id=b.player_id WHERE a.player_id=1; SELECT COUNT(*) FROM keyless_accounts; SELECT COUNT(*) FROM keyless_bonuses;")
test "$keyless_join_dml_state" = $'15\t1\n16\t1\n27\t1\n14\n1\n1'
insert_set_state=$(docker run --rm --network "${project}_default" mysql:8.0 \
  mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root \
  --batch --skip-column-names --execute="USE smoke; CREATE TABLE insert_set_rows (id BIGINT AUTO_INCREMENT PRIMARY KEY,actor VARCHAR(20) UNIQUE,value BIGINT DEFAULT 7,note VARCHAR(20)); INSERT INTO insert_set_rows SET actor='a',value=10,note=CONCAT('x','y'); INSERT INTO insert_set_rows SET actor='a',value=3 ON DUPLICATE KEY UPDATE value=value+VALUES(value),note='updated'; INSERT IGNORE INTO insert_set_rows SET actor='a',value=99; REPLACE INTO insert_set_rows SET actor='a',value=5,note='replace'; INSERT INTO insert_set_rows SET actor='b',value=DEFAULT; START TRANSACTION; INSERT INTO insert_set_rows SET actor='rollback'; ROLLBACK; SELECT id,actor,value,IFNULL(note,'NULL') FROM insert_set_rows ORDER BY id; SELECT COUNT(*) FROM insert_set_rows WHERE actor='rollback';")
test "$insert_set_state" = $'4\ta\t5\treplace\n5\tb\t7\tNULL\n0'
insert_values_state=$(docker run --rm --network "${project}_default" mysql:8.0 \
  mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root \
  --batch --skip-column-names --execute="USE smoke; CREATE TABLE insert_value_defaults (id BIGINT AUTO_INCREMENT PRIMARY KEY,actor VARCHAR(20) DEFAULT 'anon',value BIGINT DEFAULT 7,note VARCHAR(30) NULL); INSERT INTO insert_value_defaults () VALUES (); INSERT INTO insert_value_defaults VALUES (); INSERT INTO insert_value_defaults(actor,value,note) VALUES(DEFAULT,DEFAULT,CONCAT('x','y')); INSERT INTO insert_value_defaults(actor,value,note) VALUES(UPPER('bob'),1+2,IF(1=1,'yes','no')); INSERT INTO insert_value_defaults(id,actor,value,note) VALUES(DEFAULT(id),DEFAULT(actor),DEFAULT(value),CONCAT(DEFAULT(actor),'-',DEFAULT(value))); START TRANSACTION; INSERT INTO insert_value_defaults VALUES (); ROLLBACK; INSERT INTO insert_value_defaults VALUES (); SELECT id,actor,value,IFNULL(note,'NULL') FROM insert_value_defaults ORDER BY id; CREATE TABLE required_default_row (id BIGINT AUTO_INCREMENT PRIMARY KEY,required_value VARCHAR(10) NOT NULL);")
test "$insert_values_state" = $'1\tanon\t7\tNULL\n2\tanon\t7\tNULL\n3\tanon\t7\txy\n4\tBOB\t3\tyes\n5\tanon\t7\tanon-7\n7\tanon\t7\tNULL'
set +e
insert_values_required_error=$(docker run --rm --network "${project}_default" mysql:8.0 \
  mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root \
  --batch --skip-column-names --execute="USE smoke; INSERT INTO required_default_row VALUES ();" 2>&1)
insert_values_required_status=$?
set -e
test "$insert_values_required_status" -ne 0
printf '%s' "$insert_values_required_error" | grep -q "ERROR 1364 .*required_value.*doesn't have a default value"
update_default_state=$(docker run --rm --network "${project}_default" mysql:8.0 \
  mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root \
  --batch --skip-column-names --execute="USE smoke; CREATE TABLE default_updates (id BIGINT AUTO_INCREMENT PRIMARY KEY,actor VARCHAR(20) UNIQUE,value BIGINT DEFAULT 7,note VARCHAR(30) DEFAULT 'base',optional VARCHAR(10) DEFAULT NULL); INSERT INTO default_updates(actor,value,note,optional) VALUES('a',10,'custom','x'); UPDATE default_updates SET value=DEFAULT,note=CONCAT(DEFAULT(note),'-u'),optional=DEFAULT(optional) WHERE actor='a'; SELECT value,note,IFNULL(optional,'NULL') FROM default_updates; INSERT INTO default_updates(actor,value,note) VALUES('a',99,'incoming') ON DUPLICATE KEY UPDATE value=DEFAULT,note=DEFAULT(note); SELECT value,note,IFNULL(optional,'NULL') FROM default_updates; CREATE TABLE keyless_defaults (value BIGINT DEFAULT 9,note VARCHAR(20) DEFAULT 'k'); INSERT INTO keyless_defaults VALUES(1,'x'),(2,'y'); UPDATE keyless_defaults SET value=DEFAULT,note=DEFAULT(note); SELECT value,note,COUNT(*) FROM keyless_defaults GROUP BY value,note; CREATE TABLE default_marker (id BIGINT PRIMARY KEY); INSERT INTO default_marker VALUES(1); UPDATE default_updates SET note='custom' WHERE id=1; UPDATE default_updates d JOIN default_marker m ON d.id=m.id SET d.note=DEFAULT(d.note); SELECT note FROM default_updates WHERE id=1; START TRANSACTION; UPDATE default_updates SET value=99 WHERE id=1; UPDATE default_updates SET value=DEFAULT WHERE id=1; ROLLBACK; SELECT value FROM default_updates WHERE id=1; CREATE TABLE required_update (id BIGINT PRIMARY KEY,required_value VARCHAR(10) NOT NULL); INSERT INTO required_update VALUES(1,'x');")
test "$update_default_state" = $'7\tbase-u\tNULL\n7\tbase\tNULL\n9\tk\t2\nbase\n7'
set +e
update_required_error=$(docker run --rm --network "${project}_default" mysql:8.0 \
  mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root \
  --batch --skip-column-names --execute="USE smoke; UPDATE required_update SET required_value=DEFAULT WHERE id=1;" 2>&1)
update_required_status=$?
set -e
test "$update_required_status" -ne 0
printf '%s' "$update_required_error" | grep -q "ERROR 1364 .*required_value.*doesn't have a default value"
aliased_upsert_state=$(docker run --rm --network "${project}_default" mysql:8.0 \
  mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root \
  --batch --skip-column-names --execute="USE smoke; CREATE TABLE aliased_upserts (id BIGINT AUTO_INCREMENT PRIMARY KEY,actor VARCHAR(20) UNIQUE,value BIGINT DEFAULT 7,note VARCHAR(20)); INSERT INTO aliased_upserts(actor,value,note) VALUES('a',10,'base'); INSERT INTO aliased_upserts(actor,value,note) VALUES('a',3,'row') AS new ON DUPLICATE KEY UPDATE value=aliased_upserts.value+new.value,note=new.note; INSERT INTO aliased_upserts(actor,value,note) VALUES('a',2,'cols') AS new(a,v,n) ON DUPLICATE KEY UPDATE value=aliased_upserts.value+v,note=n; INSERT INTO aliased_upserts(actor,value,note) VALUES('a',1,'qualified') AS new(a,v,n) ON DUPLICATE KEY UPDATE value=aliased_upserts.value+new.v,note=new.n; INSERT INTO aliased_upserts SET actor='a',value=1,note='set' AS new ON DUPLICATE KEY UPDATE value=aliased_upserts.value+new.value,note=new.note; START TRANSACTION; INSERT INTO aliased_upserts(actor,value,note) VALUES('a',100,'rollback') AS new ON DUPLICATE KEY UPDATE value=aliased_upserts.value+new.value,note=new.note; ROLLBACK; INSERT INTO aliased_upserts(actor,value,note) VALUES('b',5,'insert') AS new ON DUPLICATE KEY UPDATE value=new.value,note=new.note; SELECT id,actor,value,note FROM aliased_upserts ORDER BY id;")
test "$aliased_upsert_state" = $'1\ta\t17\tset\n7\tb\t5\tinsert'
scalar_upsert_state=$(docker run --rm --network "${project}_default" mysql:8.0 \
  mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root \
  --batch --skip-column-names --execute="USE smoke; CREATE TABLE scalar_upserts (id BIGINT AUTO_INCREMENT PRIMARY KEY,actor VARCHAR(20) UNIQUE,value BIGINT,note VARCHAR(100),state VARCHAR(20)); INSERT INTO scalar_upserts(actor,value,note,state) VALUES('a',10,'base','old'); INSERT INTO scalar_upserts(actor,value,note,state) VALUES('a',15,'incoming','x') AS new ON DUPLICATE KEY UPDATE value=GREATEST(scalar_upserts.value,new.value)+1,note=CONCAT(scalar_upserts.note,':',new.note),state=IF(new.value>10,'raised','kept'); INSERT INTO scalar_upserts(actor,value,note,state) VALUES('a',2,'next','x') AS new ON DUPLICATE KEY UPDATE value=scalar_upserts.value+new.value,note=CONCAT(scalar_upserts.note,'-',scalar_upserts.value),state=CASE WHEN scalar_upserts.value>=18 THEN 'high' ELSE 'low' END; INSERT INTO scalar_upserts(actor,value,note,state) VALUES('a',NULL,NULL,NULL) AS new ON DUPLICATE KEY UPDATE value=COALESCE(new.value,scalar_upserts.value),note=COALESCE(new.note,scalar_upserts.note),state=COALESCE(new.state,scalar_upserts.state); SELECT id,actor,value,note,state FROM scalar_upserts;")
test "$scalar_upsert_state" = $'1\ta\t18\tbase:incoming-18\thigh'
docker run --rm --network "${project}_default" mysql:8.0 \
  mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root \
  --execute="USE smoke; CREATE TABLE affected_updates (id BIGINT PRIMARY KEY,value BIGINT,note VARCHAR(20)); INSERT INTO affected_updates VALUES(1,10,'x'),(2,20,'y'); CREATE TABLE affected_source (id BIGINT PRIMARY KEY,value BIGINT); INSERT INTO affected_source VALUES(1,11),(2,99); CREATE TABLE affected_keyless (value BIGINT,note VARCHAR(20)); INSERT INTO affected_keyless VALUES(1,'x'),(1,'x'); CREATE TABLE affected_upsert (id BIGINT AUTO_INCREMENT PRIMARY KEY,actor VARCHAR(20) UNIQUE,value BIGINT); INSERT INTO affected_upsert(actor,value) VALUES('a',1);"
affected_noop=$(docker exec mydb mydb-cli --password root --database smoke --execute="UPDATE affected_updates SET value=10 WHERE id=1")
test "$affected_noop" = "Query OK, 0 rows affected"
affected_changed=$(docker exec mydb mydb-cli --password root --database smoke --execute="UPDATE affected_updates SET value=11 WHERE id=1")
test "$affected_changed" = "Query OK, 1 rows affected"
affected_expression_noop=$(docker exec mydb mydb-cli --password root --database smoke --execute="UPDATE affected_updates SET value=value")
test "$affected_expression_noop" = "Query OK, 0 rows affected"
affected_join=$(docker exec mydb mydb-cli --password root --database smoke --execute="UPDATE affected_updates a JOIN affected_source b ON a.id=b.id SET a.value=b.value")
test "$affected_join" = "Query OK, 1 rows affected"
affected_keyless_noop=$(docker exec mydb mydb-cli --password root --database smoke --execute="UPDATE affected_keyless SET value=1,note='x'")
test "$affected_keyless_noop" = "Query OK, 0 rows affected"
affected_keyless_changed=$(docker exec mydb mydb-cli --password root --database smoke --execute="UPDATE affected_keyless SET value=2")
test "$affected_keyless_changed" = "Query OK, 2 rows affected"
affected_upsert_noop=$(docker exec mydb mydb-cli --password root --database smoke --execute="INSERT INTO affected_upsert(actor,value) VALUES('a',1) AS new ON DUPLICATE KEY UPDATE value=new.value")
test "$affected_upsert_noop" = "Query OK, 0 rows affected"
create_like_state=$(docker run --rm --network "${project}_default" mysql:8.0 \
  mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root \
  --batch --skip-column-names --execute="USE smoke; CREATE TABLE like_parent (id BIGINT PRIMARY KEY); INSERT INTO like_parent VALUES(7); CREATE TABLE like_source (id BIGINT AUTO_INCREMENT PRIMARY KEY,actor VARCHAR(32) NOT NULL UNIQUE,score BIGINT DEFAULT 7,KEY score_idx(score),CONSTRAINT chk_score CHECK(score>=0),CONSTRAINT fk_score FOREIGN KEY(score) REFERENCES like_parent(id)) ENGINE=InnoDB AUTO_INCREMENT=42; INSERT INTO like_source(actor,score) VALUES('source',7); CREATE TABLE like_copy LIKE like_source; INSERT INTO like_copy(actor,score) VALUES('copy',99); SELECT id,actor,score FROM like_copy; SELECT COUNT(*) FROM like_copy; SHOW CREATE TABLE like_copy; CREATE TABLE IF NOT EXISTS like_copy LIKE like_source; SHOW COUNT(*) WARNINGS; SHOW WARNINGS;")
printf '%s\n' "$create_like_state" | grep -q $'^1\tcopy\t99$'
printf '%s\n' "$create_like_state" | grep -q 'UNIQUE'
printf '%s\n' "$create_like_state" | grep -q 'score_idx'
printf '%s\n' "$create_like_state" | grep -q 'like_copy_chk_1'
! printf '%s\n' "$create_like_state" | grep -q 'FOREIGN KEY'
! printf '%s\n' "$create_like_state" | grep -q 'AUTO_INCREMENT=42'
printf '%s\n' "$create_like_state" | grep -q $'^Note\t1050\tTable '\''like_copy'\'' already exists$'
set +e
create_like_check_error=$(docker run --rm --network "${project}_default" mysql:8.0 \
  mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root \
  --batch --skip-column-names --execute="USE smoke; INSERT INTO like_copy(actor,score) VALUES('bad',-1);" 2>&1)
create_like_check_status=$?
set -e
test "$create_like_check_status" -ne 0
printf '%s' "$create_like_check_error" | grep -q 'ERROR 3819 .*like_copy_chk_1'
truncate_state=$(docker run --rm --network "${project}_default" mysql:8.0 \
  mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root \
  --batch --skip-column-names --execute="USE smoke; CREATE TABLE truncate_audit (id BIGINT PRIMARY KEY); CREATE TABLE truncate_auto (id BIGINT AUTO_INCREMENT PRIMARY KEY,value BIGINT); INSERT INTO truncate_auto(value) VALUES(10),(20); DELETE FROM truncate_auto; START TRANSACTION; INSERT INTO truncate_audit VALUES(1); TRUNCATE TABLE truncate_auto; ROLLBACK; INSERT INTO truncate_auto(value) VALUES(30); SELECT COUNT(*) FROM truncate_audit; SELECT COUNT(*) FROM truncate_auto; SELECT id FROM truncate_auto; CREATE TABLE truncate_parent (id BIGINT PRIMARY KEY); CREATE TABLE truncate_child (id BIGINT PRIMARY KEY,parent_id BIGINT,CONSTRAINT truncate_fk FOREIGN KEY(parent_id) REFERENCES truncate_parent(id)); INSERT INTO truncate_parent VALUES(1); INSERT INTO truncate_child VALUES(1,1);")
test "$truncate_state" = $'1\n1\n1'
set +e
truncate_fk_error=$(docker run --rm --network "${project}_default" mysql:8.0 \
  mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root \
  --batch --skip-column-names --execute="USE smoke; TRUNCATE TABLE truncate_parent;" 2>&1)
truncate_fk_status=$?
set -e
test "$truncate_fk_status" -ne 0
printf '%s' "$truncate_fk_error" | grep -q 'ERROR 1701 .*truncate_child.*truncate_fk'
truncate_fk_disabled_state=$(docker run --rm --network "${project}_default" mysql:8.0 \
  mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root \
  --batch --skip-column-names --execute="USE smoke; TRUNCATE TABLE truncate_child; SET FOREIGN_KEY_CHECKS=0; TRUNCATE TABLE truncate_parent; SET FOREIGN_KEY_CHECKS=1; SELECT COUNT(*) FROM truncate_child; SELECT COUNT(*) FROM truncate_parent;")
test "$truncate_fk_disabled_state" = $'0\n0'
load_data_state=$(docker run --rm --network "${project}_default" --entrypoint sh mysql:8.0 -c '
  printf '\''id,name,note\n101,"loader","hello,world"\n102,bob,plain\n'\'' >/tmp/mydb-load.csv
  printf '\''301,\351,\351\n302,only\n303,a,b,extra\n301,dup,x\n'\'' >/tmp/mydb-load-warnings.csv
  mysql --local-infile=1 --protocol=TCP --host=mydb --port=3306 --user=root --password=root --batch --skip-column-names --execute="USE smoke; CREATE TABLE load_players (id BIGINT PRIMARY KEY,name VARCHAR(32),note VARCHAR(64)) ENGINE=InnoDB; LOAD DATA LOCAL INFILE '\''/tmp/mydb-load.csv'\'' INTO TABLE load_players FIELDS TERMINATED BY '\'','\'' OPTIONALLY ENCLOSED BY '\''"'\'' LINES TERMINATED BY '\''\\n'\'' IGNORE 1 LINES (@raw_id,@raw_name,@raw_note) SET id=@raw_id,name=UPPER(@raw_name),note=UPPER(@raw_note); SELECT id,name,note FROM load_players ORDER BY id; CREATE TABLE load_warnings (id BIGINT PRIMARY KEY,name VARCHAR(32),raw BLOB) ENGINE=InnoDB; LOAD DATA LOCAL INFILE '\''/tmp/mydb-load-warnings.csv'\'' INTO TABLE load_warnings CHARACTER SET latin1 FIELDS TERMINATED BY '\'','\''; SHOW COUNT(*) WARNINGS; SHOW WARNINGS; SELECT id,HEX(name),IFNULL(HEX(raw),'\''NULL'\'') FROM load_warnings ORDER BY id;"
')
test "$load_data_state" = $'101\tLOADER\tHELLO,WORLD\n102\tBOB\tPLAIN\n3\nWarning\t1261\tRow 2 doesn'\''t contain data for all columns\nWarning\t1262\tRow 3 was truncated; it contained more data than there were input columns\nWarning\t1062\tDuplicate entry '\''301'\'' for key '\''load_warnings.PRIMARY'\''\n301\tC3A9\tE9\n302\t6F6E6C79\tNULL\n303\t61\t62'
docker exec mydb sh -c "printf '201\\tserver\\tsecure-file\\n' >/var/lib/mydb/imports/server-load.tsv"
server_load_data_state=$(docker run --rm --network "${project}_default" mysql:8.0 \
  mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root \
  --batch --skip-column-names --execute="USE smoke; CREATE TABLE server_load_players (id BIGINT PRIMARY KEY,name VARCHAR(32),note VARCHAR(64)) ENGINE=InnoDB; LOAD DATA INFILE '/var/lib/mydb/imports/server-load.tsv' INTO TABLE server_load_players; SELECT id,name,note FROM server_load_players;")
test "$server_load_data_state" = $'201\tserver\tsecure-file'
docker exec mydb sh -c "printf '401,a,b\\n402,only\\n403,c,d\\n' >/var/lib/mydb/imports/strict-missing.csv; printf '411,a,b\\n412,c,d,extra\\n413,e,f\\n' >/var/lib/mydb/imports/strict-extra.csv; printf '421,ok,a\\n422,\\377,b\\n423,end,c\\n' >/var/lib/mydb/imports/strict-invalid.csv"
set +e
strict_missing_error=$(docker run --rm --network "${project}_default" mysql:8.0 \
  mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root \
  --batch --skip-column-names --execute="USE smoke; CREATE TABLE strict_missing (id BIGINT PRIMARY KEY,name VARCHAR(32),raw BLOB); LOAD DATA INFILE '/var/lib/mydb/imports/strict-missing.csv' INTO TABLE strict_missing FIELDS TERMINATED BY ',';" 2>&1)
strict_missing_status=$?
strict_extra_error=$(docker run --rm --network "${project}_default" mysql:8.0 \
  mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root \
  --batch --skip-column-names --execute="USE smoke; CREATE TABLE strict_extra (id BIGINT PRIMARY KEY,name VARCHAR(32),raw BLOB); LOAD DATA INFILE '/var/lib/mydb/imports/strict-extra.csv' INTO TABLE strict_extra FIELDS TERMINATED BY ',';" 2>&1)
strict_extra_status=$?
strict_invalid_error=$(docker run --rm --network "${project}_default" mysql:8.0 \
  mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root \
  --batch --skip-column-names --execute="USE smoke; CREATE TABLE strict_invalid (id BIGINT PRIMARY KEY,name VARCHAR(32),raw BLOB); LOAD DATA INFILE '/var/lib/mydb/imports/strict-invalid.csv' IGNORE INTO TABLE strict_invalid CHARACTER SET utf8mb4 FIELDS TERMINATED BY ',';" 2>&1)
strict_invalid_status=$?
set -e
test "$strict_missing_status" -ne 0
printf '%s' "$strict_missing_error" | grep -q "ERROR 1261 .*Row 2 doesn't contain data for all columns"
test "$strict_extra_status" -ne 0
printf '%s' "$strict_extra_error" | grep -q 'ERROR 1262 .*Row 2 was truncated'
test "$strict_invalid_status" -ne 0
printf '%s' "$strict_invalid_error" | grep -q 'ERROR 1300 .*Invalid utf8mb4 character string'
strict_load_counts=$(docker run --rm --network "${project}_default" mysql:8.0 \
  mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root \
  --batch --skip-column-names --execute="USE smoke; SELECT COUNT(*) FROM strict_missing; SELECT COUNT(*) FROM strict_extra; SELECT COUNT(*) FROM strict_invalid;")
test "$strict_load_counts" = $'0\n0\n0'
set +e
outside_load_error=$(docker run --rm --network "${project}_default" mysql:8.0 \
  mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root \
  --batch --skip-column-names --execute="USE smoke; LOAD DATA INFILE '/etc/passwd' INTO TABLE server_load_players;" 2>&1)
outside_load_status=$?
set -e
test "$outside_load_status" -ne 0
printf '%s' "$outside_load_error" | grep -q 'secure_file_priv'
savepoint_expression_state=$(docker run --rm --network "${project}_default" mysql:8.0 \
  mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root \
  --batch --skip-column-names --execute="USE smoke; START TRANSACTION; SAVEPOINT before_change; UPDATE players SET score=99 WHERE id=1; SAVEPOINT after_change; ROLLBACK TO before_change; RELEASE SAVEPOINT before_change; COMMIT; SELECT score FROM players WHERE id=1; SELECT CASE WHEN score>=20 THEN CONCAT(UPPER(name),'-',CAST(score AS CHAR)) ELSE CONCAT_WS(':',name,NULL,'rookie') END,ROUND(score/3,2),CAST(score/3 AS DECIMAL(10,1)),CONVERT(score,SIGNED) FROM players WHERE id=1; SELECT id FROM players WHERE CASE WHEN CONCAT(LOWER(name),'-',CAST(score AS CHAR))='alice-10' THEN 1 ELSE 0 END=1;")
test "$savepoint_expression_state" = $'10\nalice:rookie\t3.33\t3.3\t10\n1'
scalar_mutation_state=$(docker run --rm --network "${project}_default" mysql:8.0 \
  mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root \
  --batch --skip-column-names --execute="USE smoke; SELECT id FROM players ORDER BY score DESC,LOWER(name),id DESC; CREATE TABLE scalar_smoke (id BIGINT PRIMARY KEY,name VARCHAR(32),score BIGINT,state VARCHAR(16)); INSERT INTO scalar_smoke VALUES (1,'alice',10,'low'),(2,'bob',20,'low'); START TRANSACTION; UPDATE scalar_smoke SET score=CASE WHEN LOWER(name)='alice' THEN score+5 ELSE score END,name=CONCAT(UPPER(name),'-',CAST(score AS CHAR)),state=IF(score>=15,'high','low') WHERE CONCAT(LOWER(name),'-',id)='alice-1'; SELECT name,score,state FROM scalar_smoke WHERE id=1; ROLLBACK; SELECT name,score,state FROM scalar_smoke WHERE id=1; START TRANSACTION; DELETE FROM scalar_smoke WHERE CASE WHEN score>=20 THEN 1 ELSE 0 END=1 ORDER BY LOWER(name) LIMIT 1; SELECT COUNT(*) FROM scalar_smoke; ROLLBACK; SELECT COUNT(*) FROM scalar_smoke;")
test "$scalar_mutation_state" = $'4\n2\n1\nALICE-15\t15\thigh\nalice\t10\tlow\n1\n2'
set_state=$(docker run --rm --network "${project}_default" mysql:8.0 \
  mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root \
  --batch --skip-column-names --execute="USE smoke; SELECT DISTINCT c.clan_name FROM players p CROSS JOIN clans c ORDER BY c.clan_name LIMIT 1 OFFSET 1; SELECT COUNT(DISTINCT actor_id) FROM players; SELECT c.clan_name,COUNT(*) AS n FROM players p CROSS JOIN clans c GROUP BY c.clan_name HAVING n > 2 ORDER BY c.clan_name; SELECT actor_id FROM players WHERE EXISTS (SELECT actor_id,clan_name FROM clans WHERE clan_name='red') ORDER BY id LIMIT 1;")
test "$set_state" = $'red\n3\nblue\t3\nred\t3\na1'
docker run --rm --network "${project}_default" mysql:8.0 \
  mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root \
  --batch --skip-column-names \
  --execute="START TRANSACTION; UPDATE smoke.ru_probe SET value=99 WHERE id=1; SELECT SLEEP(4); ROLLBACK;" \
  >/dev/null &
ru_writer_pid=$!
sleep 1
ru_dirty=$(docker run --rm --network "${project}_default" mysql:8.0 \
  mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root \
  --batch --skip-column-names \
  --execute="SET SESSION TRANSACTION ISOLATION LEVEL READ UNCOMMITTED; START TRANSACTION; SELECT value FROM smoke.ru_probe WHERE id=1; COMMIT;")
test "$ru_dirty" = "99"
wait "$ru_writer_pid"
ru_after=$(docker run --rm --network "${project}_default" mysql:8.0 \
  mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root \
  --batch --skip-column-names --execute="SELECT value FROM smoke.ru_probe WHERE id=1;")
test "$ru_after" = "10"
docker run --rm --network "${project}_default" mysql:8.0 \
  mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root \
  --execute="USE smoke; CREATE TABLE share_lock_probe (id BIGINT PRIMARY KEY,value BIGINT); INSERT INTO share_lock_probe VALUES(1,10),(2,20);"
docker run --rm --network "${project}_default" mysql:8.0 \
  mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root \
  --batch --skip-column-names \
  --execute="USE smoke; START TRANSACTION; SELECT value FROM share_lock_probe WHERE id=1 FOR SHARE; SELECT SLEEP(4); COMMIT;" \
  >/dev/null &
share_locker_pid=$!
sleep 1
share_nowait=$(docker run --rm --network "${project}_default" mysql:8.0 \
  mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root \
  --batch --skip-column-names \
  --execute="USE smoke; START TRANSACTION; SELECT value FROM share_lock_probe WHERE id=1 FOR SHARE NOWAIT; COMMIT;")
test "$share_nowait" = "10"
set +e
update_nowait_error=$(docker run --rm --network "${project}_default" mysql:8.0 \
  mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root \
  --batch --skip-column-names \
  --execute="USE smoke; START TRANSACTION; SELECT value FROM share_lock_probe WHERE id=1 FOR UPDATE NOWAIT; COMMIT;" 2>&1)
update_nowait_status=$?
set -e
test "$update_nowait_status" -ne 0
printf '%s' "$update_nowait_error" | grep -q 'ERROR 3572 .*NOWAIT is set'
skip_update=$(docker run --rm --network "${project}_default" mysql:8.0 \
  mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root \
  --batch --skip-column-names \
  --execute="USE smoke; START TRANSACTION; SELECT id FROM share_lock_probe ORDER BY id LIMIT 1 FOR UPDATE SKIP LOCKED; COMMIT;")
test "$skip_update" = "2"
skip_share=$(docker run --rm --network "${project}_default" mysql:8.0 \
  mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root \
  --batch --skip-column-names \
  --execute="USE smoke; START TRANSACTION; SELECT id FROM share_lock_probe ORDER BY id LIMIT 2 FOR SHARE SKIP LOCKED; COMMIT;")
test "$skip_share" = $'1\n2'
unlocked_row=$(docker run --rm --network "${project}_default" mysql:8.0 \
  mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root \
  --batch --skip-column-names \
  --execute="USE smoke; UPDATE share_lock_probe SET value=value+1 WHERE id=2; SELECT value FROM share_lock_probe WHERE id=2;")
test "$unlocked_row" = "21"
wait "$share_locker_pid"
docker run --rm --network "${project}_default" mysql:8.0 \
  mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root \
  --execute="USE smoke; CREATE TABLE deadlock_probe (id BIGINT PRIMARY KEY,value BIGINT); INSERT INTO deadlock_probe VALUES(1,1),(2,2);"
deadlock_first_log=$(mktemp)
deadlock_second_log=$(mktemp)
docker run --rm --network "${project}_default" mysql:8.0 \
  mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root \
  --batch --skip-column-names \
  --execute="USE smoke; START TRANSACTION; UPDATE deadlock_probe SET value=value+10 WHERE id=1; SELECT SLEEP(3); UPDATE deadlock_probe SET value=value+10 WHERE id=2; COMMIT;" \
  >"$deadlock_first_log" 2>&1 &
deadlock_first_pid=$!
sleep 1
docker run --rm --network "${project}_default" mysql:8.0 \
  mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root \
  --batch --skip-column-names \
  --execute="USE smoke; START TRANSACTION; UPDATE deadlock_probe SET value=value+100 WHERE id=2; SELECT SLEEP(1); UPDATE deadlock_probe SET value=value+100 WHERE id=1; COMMIT;" \
  >"$deadlock_second_log" 2>&1 &
deadlock_second_pid=$!
set +e
wait "$deadlock_first_pid"
deadlock_first_status=$?
wait "$deadlock_second_pid"
deadlock_second_status=$?
set -e
test "$deadlock_first_status" -ne 0
grep -q 'ERROR 1213 .*Deadlock found when trying to get lock' "$deadlock_first_log"
test "$deadlock_second_status" -eq 0
rm -f "$deadlock_first_log" "$deadlock_second_log"
deadlock_state=$(docker run --rm --network "${project}_default" mysql:8.0 \
  mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root \
  --batch --skip-column-names --execute="USE smoke; SELECT id,value FROM deadlock_probe ORDER BY id;")
test "$deadlock_state" = $'1\t101\n2\t102'
curl --fail --silent "http://${smoke_host}:14316/metrics" | grep -Eq 'mydb_deadlocks_total [1-9]'

docker run --rm --network "${project}_default" mysql:8.0 \
  mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root \
  --execute="USE smoke; CREATE TABLE crash_probe (id BIGINT PRIMARY KEY,value BIGINT); INSERT INTO crash_probe VALUES(1,10);"
crash_writer_log=$(mktemp)
docker run --rm --network "${project}_default" mysql:8.0 \
  mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root \
  --batch --skip-column-names \
  --execute="USE smoke; START TRANSACTION; UPDATE crash_probe SET value=99 WHERE id=1; SELECT SLEEP(30); COMMIT;" \
  >"$crash_writer_log" 2>&1 &
crash_writer_pid=$!
sleep 1
crash_dirty=$(docker run --rm --network "${project}_default" mysql:8.0 \
  mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root \
  --batch --skip-column-names \
  --execute="SET SESSION TRANSACTION ISOLATION LEVEL READ UNCOMMITTED; START TRANSACTION; SELECT value FROM smoke.crash_probe WHERE id=1; COMMIT;")
test "$crash_dirty" = "99"
docker kill --signal=KILL mydb >/dev/null
set +e
wait "$crash_writer_pid"
crash_writer_status=$?
set -e
test "$crash_writer_status" -ne 0
rm -f "$crash_writer_log"
sleep 1
if [ "$(docker inspect --format '{{.State.Status}}' mydb 2>/dev/null || true)" != "running" ]; then
  docker start mydb >/dev/null
fi
deadline=$((SECONDS + 120))
until [ "$(docker inspect --format '{{.State.Health.Status}}' mydb 2>/dev/null || true)" = "healthy" ]; do
  if [ "$SECONDS" -ge "$deadline" ]; then
    echo "MyDB container did not recover after SIGKILL" >&2
    exit 1
  fi
  sleep 2
done
crash_state=$(docker run --rm --network "${project}_default" mysql:8.0 \
  mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root \
  --batch --skip-column-names \
  --execute="USE smoke; SELECT value FROM crash_probe WHERE id=1; START TRANSACTION; UPDATE crash_probe SET value=value+1 WHERE id=1; COMMIT; SELECT value FROM crash_probe WHERE id=1;")
test "$crash_state" = $'10\n11'
docker stop --time 30 mydb >/dev/null
wal_injection=$(docker run --rm --volumes-from mydb --entrypoint sh mydb:dev -c '
  wal=$(ls -1 /var/lib/mydb/data/wal/wal_*.log | sort | tail -n 1)
  before=$(wc -c < "$wal")
  printf "\001\002\003\004\005" >> "$wal"
  after=$(wc -c < "$wal")
  printf "%s\t%s\t%s\n" "$wal" "$before" "$after"
')
IFS=$'\t' read -r wal_path wal_valid_length wal_injected_length <<<"$wal_injection"
test "$wal_injected_length" -eq $((wal_valid_length + 5))
docker start mydb >/dev/null
deadline=$((SECONDS + 120))
until [ "$(docker inspect --format '{{.State.Health.Status}}' mydb 2>/dev/null || true)" = "healthy" ]; do
  if [ "$SECONDS" -ge "$deadline" ]; then
    echo "MyDB did not recover the WAL torn tail" >&2
    exit 1
  fi
  sleep 2
done
wal_recovered_length=$(docker run --rm --volumes-from mydb --entrypoint sh mydb:dev -c "wc -c < '$wal_path'")
test "$wal_recovered_length" -eq "$wal_valid_length"
wal_recovery_state=$(docker run --rm --network "${project}_default" mysql:8.0 \
  mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root \
  --batch --skip-column-names \
  --execute="USE smoke; SELECT value FROM crash_probe WHERE id=1; UPDATE crash_probe SET value=value+1 WHERE id=1; SELECT value FROM crash_probe WHERE id=1;")
test "$wal_recovery_state" = $'11\n12'
persisted_state=$(docker run --rm --network "${project}_default" mysql:8.0 \
  mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root \
  --batch --skip-column-names --execute="USE smoke; SELECT COUNT(*) FROM players; SELECT name,score FROM players WHERE actor_id='a2';")
test "$persisted_state" = $'3\nbob2\t21'
memory_state=$(docker run --rm --network "${project}_default" mysql:8.0 \
  mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root \
  --batch --skip-column-names --execute="USE smoke; SELECT COUNT(*) FROM volatile_sessions; SELECT COUNT(*) FROM information_schema.tables WHERE table_schema=DATABASE() AND table_name='volatile_sessions'; SHOW CREATE TABLE volatile_sessions;")
test "$(sed -n '1,2p' <<<"$memory_state")" = $'0\n1'
grep -Eq '^volatile_sessions[[:space:]].*ENGINE=MEMORY' <<<"$memory_state"
schema_rows=$(docker run --rm --network "${project}_default" mysql:8.0 \
  mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root \
  --batch --skip-column-names --execute="USE smoke; SELECT COUNT(*) FROM information_schema.tables WHERE table_schema=DATABASE() AND table_name='players';")
test "$schema_rows" = "1"
actor_counter_state=$(docker run --rm --network "${project}_default" mysql:8.0 \
  mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root \
  --batch --skip-column-names --execute="USE smoke; UPDATE players SET score=10 WHERE id=1; UPDATE players SET score=score+0.5,score=score*2 WHERE id=1; INSERT INTO players (id,actor_id,name,score) VALUES (1,'a1','ignored',3) ON DUPLICATE KEY UPDATE score=score+VALUES(score); SELECT score FROM players WHERE id=1;")
test "$actor_counter_state" = "25"
fk_state=$(docker run --rm --network "${project}_default" mysql:8.0 \
  mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root \
  --batch --skip-column-names --execute="USE smoke; CREATE TABLE fk_accounts (id BIGINT PRIMARY KEY); CREATE TABLE fk_items (id BIGINT PRIMARY KEY, account_id BIGINT, amount BIGINT, payload BLOB, CONSTRAINT fk_item_account FOREIGN KEY (account_id) REFERENCES fk_accounts (id) ON DELETE CASCADE ON UPDATE RESTRICT, CONSTRAINT chk_item_amount CHECK (amount > 0)); INSERT INTO fk_accounts VALUES (1); INSERT INTO fk_items VALUES (10,1,2,0x00FF5C27); SELECT HEX(payload) FROM fk_items WHERE id=10;")
test "$fk_state" = "00FF5C27"
set +e
fk_error=$(docker run --rm --network "${project}_default" mysql:8.0 \
  mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root \
  --execute="USE smoke; INSERT INTO fk_items VALUES (11,999,1,NULL);" 2>&1)
fk_status=$?
set -e
if [ "$fk_status" -eq 0 ] || ! grep -q 'ERROR 1452' <<<"$fk_error"; then
  echo "foreign key rejection check failed" >&2
  exit 1
fi
set +e
check_error=$(docker run --rm --network "${project}_default" mysql:8.0 \
  mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root \
  --execute="USE smoke; INSERT INTO fk_items VALUES (12,1,0,NULL);" 2>&1)
check_status=$?
set -e
if [ "$check_status" -eq 0 ] || ! grep -q 'ERROR 3819' <<<"$check_error"; then
  echo "CHECK constraint rejection failed" >&2
  exit 1
fi
fk_switch=$(docker run --rm --network "${project}_default" mysql:8.0 \
  mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root \
  --batch --skip-column-names --execute="SET @OLD_FOREIGN_KEY_CHECKS=@@FOREIGN_KEY_CHECKS, FOREIGN_KEY_CHECKS=0; SELECT @@FOREIGN_KEY_CHECKS; SET FOREIGN_KEY_CHECKS=@OLD_FOREIGN_KEY_CHECKS; SELECT @@FOREIGN_KEY_CHECKS;")
test "$fk_switch" = $'0\n1'
fk_cascade=$(docker run --rm --network "${project}_default" mysql:8.0 \
  mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root \
  --batch --skip-column-names --execute="USE smoke; START TRANSACTION; DELETE FROM fk_accounts WHERE id=1; SELECT COUNT(*) FROM fk_items; ROLLBACK; SELECT COUNT(*) FROM fk_items; DELETE FROM fk_accounts WHERE id=1; SELECT COUNT(*) FROM fk_items; INSERT INTO fk_accounts VALUES (2); INSERT INTO fk_items VALUES (20,2,3,0xDEADBEEF); START TRANSACTION; SET FOREIGN_KEY_CHECKS=0; INSERT INTO fk_items VALUES (30,999,1,NULL); SET FOREIGN_KEY_CHECKS=1; INSERT INTO fk_accounts VALUES (3); INSERT INTO fk_items VALUES (31,3,1,NULL); COMMIT; SELECT COUNT(*) FROM fk_items WHERE id IN (30,31);")
test "$fk_cascade" = $'0\n1\n0\n2'
ordered_mutation_state=$(docker run --rm --network "${project}_default" mysql:8.0 \
  mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root \
  --batch --skip-column-names --execute="USE smoke; CREATE TABLE mutation_queue (id BIGINT PRIMARY KEY, value BIGINT); INSERT INTO mutation_queue VALUES (1,1),(2,2),(3,3),(4,4); UPDATE mutation_queue SET value=value+10 ORDER BY id DESC LIMIT 2; DELETE FROM mutation_queue ORDER BY value ASC LIMIT 1; UPDATE mutation_queue SET id=id+10 ORDER BY id DESC; SELECT id,value FROM mutation_queue ORDER BY id; CREATE TABLE duplicate_events (value BIGINT, note VARCHAR(10)); INSERT INTO duplicate_events VALUES (1,'a'),(1,'a'),(1,'a'); UPDATE duplicate_events SET value=9 WHERE value=1 LIMIT 1; DELETE FROM duplicate_events WHERE value=1 LIMIT 1; SELECT value,COUNT(*) FROM duplicate_events GROUP BY value ORDER BY value;")
test "$ordered_mutation_state" = $'12\t2\n13\t13\n14\t14\n1\t1\n9\t1'
metrics=$(curl --fail --silent "http://${smoke_host}:14316/metrics")
grep -q "mydb_up 1" <<<"$metrics"
grep -Eq "mydb_row_lock_acquires_total [1-9]" <<<"$metrics"
grep -Eq "mydb_wal_sync_microseconds_total [1-9]" <<<"$metrics"
grep -q "mydb_checkpoint_errors_total 0" <<<"$metrics"
curl --fail --silent -H "Authorization: Bearer root" \
  "http://${smoke_host}:14316/api/v1/agent/health" | grep -q '"checkpoint_errors"'
curl --fail --silent -X POST -H "Authorization: Bearer root" -H "Content-Type: application/json" \
  -d '{"question":"为什么写入延迟高？"}' \
  "http://${smoke_host}:14316/api/v1/agent/ask" | grep -q '"intent":"write_latency"'

full_json=$(curl --fail --silent -X POST -H "Authorization: Bearer root" \
  "http://${smoke_host}:14316/api/v1/backup/full")
full_id=$(sed -n 's/.*"id":"\([^"]*\)".*/\1/p' <<<"$full_json")
test -n "$full_id"
docker run --rm --network "${project}_default" mysql:8.0 \
  mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root \
  --batch --skip-column-names --execute="USE smoke; INSERT INTO players (actor_id,name,score) VALUES ('a4','delta',40);"
point_in_time=$(date -u +"%Y-%m-%dT%H:%M:%S.999Z")
sleep 1.2
docker run --rm --network "${project}_default" mysql:8.0 \
  mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root \
  --batch --skip-column-names --execute="USE smoke; INSERT INTO players (actor_id,name,score) VALUES ('a5','after_target',50);"
incremental_json=$(curl --fail --silent -X POST -H "Authorization: Bearer root" \
  -H "Content-Type: application/json" -d "{\"base_id\":\"$full_id\"}" \
  "http://${smoke_host}:14316/api/v1/backup/incremental")
incremental_id=$(sed -n 's/.*"id":"\([^"]*\)".*/\1/p' <<<"$incremental_json")
test -n "$incremental_id"
curl --fail --silent -X POST -H "Authorization: Bearer root" \
  -H "Content-Type: application/json" \
  -d "{\"id\":\"$incremental_id\",\"point_in_time\":\"$point_in_time\"}" \
  "http://${smoke_host}:14316/api/v1/backup/restore" | grep -q '"restart_required":true'
docker compose -p "$project" restart mydb
deadline=$((SECONDS + 120))
until [ "$(docker inspect --format '{{.State.Health.Status}}' mydb 2>/dev/null || true)" = "healthy" ]; do
  if [ "$SECONDS" -ge "$deadline" ]; then
    echo "MyDB did not recover the backup chain" >&2
    exit 1
  fi
  sleep 2
done
backup_state=$(docker run --rm --network "${project}_default" mysql:8.0 \
  mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root \
  --batch --skip-column-names --execute="USE smoke; SELECT COUNT(*) FROM players WHERE actor_id='a4'; SELECT COUNT(*) FROM players WHERE actor_id='a5'; SELECT account_id,amount,HEX(payload) FROM fk_items WHERE id=20;")
test "$backup_state" = $'1\n0\n2\t3\tDEADBEEF'
echo "Docker smoke passed: official/native MySQL CLI, changed-row affected counts/no-op WAL avoidance, SIGKILL committed/uncommitted and WAL torn-tail recovery, INSERT/REPLACE SET, INSERT VALUES default rows/expressions/1364, UPDATE/UPSERT/JOIN DEFAULT, MySQL 8 row/column alias and complex scalar/left-to-right UPSERT, CREATE TABLE LIKE, TRUNCATE implicit commit/auto-ID/FK 1701, FOR SHARE/NOWAIT 3572/SKIP LOCKED/deadlock 1213 rollback, LOAD DATA LOCAL/secure INFILE charset/warnings/strict atomic errors, transaction/SAVEPOINT/RU dirty read, MEMORY restart, advanced JOIN+aggregate+correlated/derived/CTE subquery, UNION/INTERSECT/EXCEPT, window/common scalar expressions, keyed/keyless multi-target JOIN UPDATE/DELETE, multi/expression ORDER BY, scalar and atomic actor UPDATE/DELETE/UPSERT, FK/CHECK/hex BLOB, row locks, auto-ID, predicates, schema, persistence, metrics, Agent NL/HTTP, full+LSN incremental PITR"
