#Requires -Version 5.1
param([switch]$Keep, [switch]$NoBuild)

$ErrorActionPreference = "Stop"
if ($PSVersionTable.PSVersion.Major -ge 7) {
    $PSNativeCommandUseErrorActionPreference = $false
}
$project = "mydb-smoke"
$env:MYDB_HOST_PORT = "13316"
$env:MYDB_HOST_HTTP_PORT = "14316"
$env:MYDB_ROOT_PASSWORD = "root"
$env:MYDB_ADMIN_PASSWORD = "root"
$env:MYDB_ENFORCE_STRONG_PASSWORDS = "false"

function Wait-MydbHealthy([string]$failureMessage) {
    $deadline = (Get-Date).AddMinutes(2)
    do {
        $health = ((docker inspect --format '{{.State.Health.Status}}' mydb 2>$null) -join "").Trim()
        if ($health -eq "healthy") { return }
        $state = ((docker inspect --format '{{.State.Status}}' mydb 2>$null) -join "").Trim()
        if ($state -eq "exited") {
            docker logs mydb
            throw "$failureMessage (container exited before healthcheck)"
        }
        Start-Sleep -Seconds 2
    } while ((Get-Date) -lt $deadline)
    docker logs mydb
    throw "$failureMessage (health=$health, state=$state)"
}

try {
    docker info | Out-Null
    if ($NoBuild) {
        docker compose -p $project up -d --no-build
    } else {
        docker compose -p $project up -d --build
    }
    if ($LASTEXITCODE -ne 0) { throw "docker compose up failed" }

    Wait-MydbHealthy "MyDB container did not become healthy"
    docker exec mydb mydbdump --help | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "mydbdump is missing from image" }
    $agentCli = docker exec mydb mydb-cli --admin-password root agent health
    if ($LASTEXITCODE -ne 0 -or ($agentCli -join "`n") -notmatch '"healthy": true') {
        throw "native Agent CLI check failed"
    }

    $sql = @"
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
SELECT COUNT(*) FROM information_schema.tables WHERE table_schema=DATABASE() AND table_name='players';
"@
    docker run --rm --network "${project}_default" mysql:8.0 `
        mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root `
        --default-character-set=utf8mb4 --execute=$sql
    if ($LASTEXITCODE -ne 0) { throw "MySQL CLI smoke test failed" }
    $nativeCliRows = docker exec mydb mydb-cli -h 127.0.0.1 -P 3306 -u root -p root `
        -D smoke -e "SELECT COUNT(*) FROM players"
    if ($LASTEXITCODE -ne 0 -or ($nativeCliRows -join "`n") -notmatch "(?m)^3$") {
        throw "native SQL CLI check failed"
    }
    $joinState = docker run --rm --network "${project}_default" mysql:8.0 `
        mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root `
        --batch --skip-column-names --execute="USE smoke; SELECT p.actor_id,c.clan_name FROM players p INNER JOIN clans c ON c.actor_id=p.actor_id WHERE c.clan_name='red'; SELECT p.actor_id,c.clan_name FROM players p LEFT OUTER JOIN clans c ON p.actor_id=c.actor_id WHERE p.actor_id IN ('a1','a3') ORDER BY p.id; SELECT p.actor_id,c.actor_id,c.clan_name FROM players p RIGHT JOIN clans c ON p.actor_id=c.actor_id ORDER BY c.actor_id; SELECT COUNT(*) FROM players p CROSS JOIN clans c; SELECT COUNT(*),COUNT(c.clan_name),SUM(p.score) FROM players p LEFT JOIN clans c ON p.actor_id=c.actor_id; SELECT c.clan_name,COUNT(*),SUM(p.score) FROM players p LEFT JOIN clans c ON p.actor_id=c.actor_id GROUP BY c.clan_name ORDER BY c.clan_name; SELECT actor_id FROM players WHERE actor_id IN (SELECT actor_id FROM clans WHERE clan_name='red'); SELECT name FROM players WHERE actor_id=(SELECT actor_id FROM clans WHERE clan_name='red');"
    if ($LASTEXITCODE -ne 0 -or ($joinState -join "`n") -ne "a1`tred`na1`tred`na3`tNULL`na1`ta1`tred`nNULL`ta9`tblue`n6`n3`t1`t61`nNULL`t2`t51`nred`t1`t10`na1`nalice") {
        throw "JOIN compatibility check failed"
    }
    $multiJoinState = docker run --rm --network "${project}_default" mysql:8.0 `
        mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root `
        --batch --skip-column-names --execute="USE smoke; SELECT p.actor_id,g.name,r.title FROM players p JOIN multi_members m ON m.actor_id=p.actor_id JOIN multi_guilds g ON g.id=m.guild_id JOIN multi_ranks r ON r.id=m.rank_id ORDER BY p.id; SELECT p.actor_id,g.name,r.title FROM players p LEFT JOIN multi_members m ON m.actor_id=p.actor_id LEFT JOIN multi_guilds g ON g.id=m.guild_id LEFT JOIN multi_ranks r ON r.id=m.rank_id ORDER BY p.id; SELECT p.actor_id,m.guild_id,g.id FROM players p JOIN multi_members m ON m.actor_id=p.actor_id RIGHT JOIN multi_guilds g ON g.id=m.guild_id ORDER BY g.id; SELECT COUNT(*) FROM players p CROSS JOIN multi_guilds g JOIN multi_members m ON m.actor_id=p.actor_id AND m.guild_id=g.id; SELECT COUNT(*) FROM players p CROSS JOIN multi_guilds g CROSS JOIN multi_ranks r;"
    if ($LASTEXITCODE -ne 0 -or ($multiJoinState -join "`n") -ne "a1`tred`tleader`na1`tred`tleader`na2`tNULL`tNULL`na3`tNULL`tNULL`na1`t1`t1`nNULL`tNULL`t3`n1`n12") {
        throw "chained multi-table JOIN compatibility check failed"
    }
    $advancedJoinState = docker run --rm --network "${project}_default" mysql:8.0 `
        mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root `
        --batch --skip-column-names --execute="USE smoke; SELECT * FROM players NATURAL LEFT JOIN multi_members ORDER BY id; SELECT * FROM players NATURAL RIGHT JOIN multi_members ORDER BY actor_id; SELECT p.actor_id,r.id FROM players p JOIN multi_ranks r ON p.score < r.id ORDER BY p.id; SELECT p.actor_id,r.id FROM players p JOIN multi_ranks r ON (p.score < r.id AND p.id <> r.id) OR p.score <=> r.id ORDER BY p.id; SELECT * FROM players JOIN multi_members USING (actor_id) ORDER BY id;"
    if ($LASTEXITCODE -ne 0 -or ($advancedJoinState -join "`n") -ne "a1`t1`talice`t10`t1`t10`na2`t2`tbob2`t21`t99`t20`na3`t4`tcat`t30`tNULL`tNULL`na1`t1`t10`t1`talice`t10`na2`t99`t20`t2`tbob2`t21`na1`t30`na2`t30`na1`t10`na1`t30`na2`t30`na3`t30`na1`t1`talice`t10`t1`t10`na2`t2`tbob2`t21`t99`t20") {
        throw "NATURAL/non-equality/composite JOIN compatibility check failed"
    }
    $correlatedState = docker run --rm --network "${project}_default" mysql:8.0 `
        mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root `
        --batch --skip-column-names --execute="USE smoke; SELECT p.actor_id FROM players p WHERE EXISTS (SELECT 1 FROM multi_members m WHERE m.actor_id=p.actor_id) ORDER BY p.id; SELECT p.actor_id FROM players p WHERE NOT EXISTS (SELECT 1 FROM multi_members m WHERE m.actor_id=p.actor_id) ORDER BY p.id; SELECT p.actor_id FROM players p WHERE p.actor_id IN (SELECT m.actor_id FROM multi_members m WHERE m.guild_id=p.id) ORDER BY p.id; SELECT p.actor_id FROM players p WHERE p.actor_id NOT IN (SELECT m.actor_id FROM multi_members m WHERE m.guild_id=p.id) ORDER BY p.id; SELECT p.actor_id FROM players p WHERE p.score = (SELECT r.id FROM multi_ranks r WHERE r.id=p.score) ORDER BY p.id; SELECT p.actor_id,g.id FROM players p CROSS JOIN multi_guilds g WHERE EXISTS (SELECT 1 FROM multi_members m WHERE m.actor_id=p.actor_id AND m.guild_id=g.id) ORDER BY p.id; SELECT p.actor_id FROM players p WHERE p.score BETWEEN 10 AND 10 AND EXISTS (SELECT 1 FROM multi_members m WHERE m.actor_id=p.actor_id); SELECT p.actor_id FROM players p WHERE (p.score=10 AND EXISTS (SELECT 1 FROM multi_members m WHERE m.actor_id=p.actor_id)) OR p.score=21 ORDER BY p.id;"
    if ($LASTEXITCODE -ne 0 -or ($correlatedState -join "`n") -ne "a1`na2`na3`na1`na2`na3`na1`na3`na1`t1`na1`na1`na2") {
        throw "correlated EXISTS/IN/scalar subquery compatibility check failed"
    }
    $derivedState = docker run --rm --network "${project}_default" mysql:8.0 `
        mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root `
        --batch --skip-column-names --execute="USE smoke; SELECT d.actor_id,d.score FROM (SELECT actor_id,score FROM players WHERE score>=20) d WHERE d.score>20 ORDER BY d.actor_id; SELECT d.score,COUNT(*) FROM (SELECT score FROM players) d GROUP BY d.score ORDER BY d.score; SELECT d.actor_id,c.clan_name FROM (SELECT actor_id,score FROM players) d LEFT JOIN clans c ON c.actor_id=d.actor_id ORDER BY d.score; SELECT d.actor_id FROM (SELECT actor_id FROM players) d WHERE EXISTS (SELECT 1 FROM multi_members m WHERE m.actor_id=d.actor_id) ORDER BY d.actor_id;"
    if ($LASTEXITCODE -ne 0 -or ($derivedState -join "`n") -ne "a2`t21`na3`t30`n10`t1`n21`t1`n30`t1`na1`tred`na2`tNULL`na3`tNULL`na1`na2") {
        throw "derived table filter/aggregate/JOIN/correlation check failed"
    }
    $unionState = docker run --rm --network "${project}_default" mysql:8.0 `
        mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root `
        --batch --skip-column-names --execute="USE smoke; SELECT actor_id AS k FROM players WHERE id=1 UNION SELECT actor_id FROM players ORDER BY k; SELECT actor_id FROM players UNION ALL SELECT actor_id FROM players ORDER BY actor_id DESC LIMIT 4; SELECT d.actor_id FROM (SELECT actor_id FROM players WHERE id=1 UNION ALL SELECT actor_id FROM players WHERE id=2) d ORDER BY d.actor_id DESC;"
    if ($LASTEXITCODE -ne 0 -or ($unionState -join "`n") -ne "a1`na2`na3`na3`na3`na2`na2`na2`na1") {
        throw "UNION DISTINCT/ALL/order/limit/derived check failed"
    }
    $setOperationState = docker run --rm --network "${project}_default" mysql:8.0 `
        mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root `
        --batch --skip-column-names --execute="USE smoke; SELECT actor_id FROM players INTERSECT SELECT actor_id FROM clans ORDER BY actor_id; SELECT actor_id FROM players EXCEPT SELECT actor_id FROM clans ORDER BY actor_id; SELECT actor_id FROM players WHERE actor_id='a1' UNION ALL SELECT actor_id FROM players INTERSECT SELECT actor_id FROM multi_members WHERE guild_id=99 ORDER BY actor_id; WITH base AS (SELECT actor_id,score FROM players WHERE score>=20), memberships(actor_id,guild_id) AS (SELECT actor_id,guild_id FROM multi_members) SELECT b.actor_id,m.guild_id FROM base b LEFT JOIN memberships m ON b.actor_id=m.actor_id ORDER BY b.actor_id; SELECT guild_id,rank_id,COUNT(*) FROM multi_members GROUP BY guild_id,rank_id ORDER BY guild_id; WITH RECURSIVE seq(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM seq WHERE n<3) SELECT n FROM seq ORDER BY n;"
    if ($LASTEXITCODE -ne 0 -or ($setOperationState -join "`n") -ne "a1`na2`na3`na1`na2`na2`t99`na3`tNULL`n1`t10`t1`n99`t20`t1`n1`n2`n3") {
        throw "INTERSECT/EXCEPT/CTE/multi-column GROUP BY check failed"
    }
    $windowState = docker run --rm --network "${project}_default" mysql:8.0 `
        mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root `
        --batch --skip-column-names --execute="USE smoke; SELECT actor_id,ROW_NUMBER() OVER (ORDER BY score DESC) AS rn,SUM(score) OVER (ORDER BY id ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) AS running,LAG(score,1,-1) OVER (ORDER BY id) AS prev FROM players ORDER BY id; SELECT rank_id,COUNT(*),SUM(COUNT(*)) OVER (ORDER BY rank_id) FROM multi_members GROUP BY rank_id ORDER BY rank_id;"
    if ($LASTEXITCODE -ne 0 -or ($windowState -join "`n") -ne "a1`t3`t10`t-1`na2`t2`t31`t10`na3`t1`t61`t21`n10`t1`t1`n20`t1`t2") {
        throw "window function partition/order/frame check failed"
    }
    $joinDmlState = docker run --rm --network "${project}_default" mysql:8.0 `
        mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root `
        --batch --skip-column-names --execute="USE smoke; CREATE TABLE join_players (id BIGINT PRIMARY KEY,value BIGINT); CREATE TABLE join_bonuses (id BIGINT PRIMARY KEY,player_id BIGINT,amount BIGINT); CREATE TABLE join_archive (id BIGINT PRIMARY KEY,value BIGINT); INSERT INTO join_players VALUES (1,10),(2,20); INSERT INTO join_bonuses VALUES (1,1,5),(2,2,7); INSERT INTO join_archive SELECT id,value FROM join_players; UPDATE join_players p JOIN join_bonuses b ON p.id=b.player_id SET p.value=p.value+b.amount; DELETE p FROM join_players p JOIN join_bonuses b ON p.id=b.player_id WHERE b.amount=7; SELECT id,value FROM join_players ORDER BY id; SELECT id,value FROM join_archive ORDER BY id;"
    if ($LASTEXITCODE -ne 0 -or ($joinDmlState -join "`n") -ne "1`t15`n1`t10`n2`t20") {
        throw "INSERT SELECT/UPDATE JOIN/DELETE JOIN check failed"
    }
    $multiTargetDmlState = docker run --rm --network "${project}_default" mysql:8.0 `
        mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root `
        --batch --skip-column-names --execute="USE smoke; CREATE TABLE multi_update_accounts (id BIGINT PRIMARY KEY,value BIGINT); CREATE TABLE multi_update_bonuses (id BIGINT PRIMARY KEY,account_id BIGINT,amount BIGINT); INSERT INTO multi_update_accounts VALUES (1,10),(2,20); INSERT INTO multi_update_bonuses VALUES (11,1,5),(12,2,7); UPDATE multi_update_accounts a JOIN multi_update_bonuses b ON a.id=b.account_id SET a.value=a.value+b.amount,b.amount=b.amount+1; SELECT a.id,a.value,b.amount FROM multi_update_accounts a JOIN multi_update_bonuses b ON a.id=b.account_id ORDER BY a.id; START TRANSACTION; DELETE a,b FROM multi_update_accounts a JOIN multi_update_bonuses b ON a.id=b.account_id WHERE a.id=1; SELECT COUNT(*) FROM multi_update_accounts; SELECT COUNT(*) FROM multi_update_bonuses; ROLLBACK; DELETE FROM a,b USING multi_update_accounts a JOIN multi_update_bonuses b ON a.id=b.account_id; SELECT COUNT(*) FROM multi_update_accounts; SELECT COUNT(*) FROM multi_update_bonuses;"
    if ($LASTEXITCODE -ne 0 -or ($multiTargetDmlState -join "`n") -ne "1`t15`t6`n2`t27`t8`n1`n1`n0`n0") {
        throw "multi-target UPDATE/DELETE compatibility check failed"
    }
    $keylessJoinDmlState = docker run --rm --network "${project}_default" mysql:8.0 `
        mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root `
        --batch --skip-column-names --execute="USE smoke; CREATE TABLE keyless_accounts (player_id BIGINT,value BIGINT); CREATE TABLE keyless_bonuses (player_id BIGINT,amount BIGINT); INSERT INTO keyless_accounts VALUES (1,10),(1,10),(2,20); INSERT INTO keyless_bonuses VALUES (1,5),(2,7); START TRANSACTION; UPDATE keyless_accounts a JOIN keyless_bonuses b ON a.player_id=b.player_id SET a.value=a.value+b.amount,b.amount=b.amount+1; SELECT value,COUNT(*) FROM keyless_accounts GROUP BY value ORDER BY value; SELECT SUM(amount) FROM keyless_bonuses; ROLLBACK; DELETE a,b FROM keyless_accounts a JOIN keyless_bonuses b ON a.player_id=b.player_id WHERE a.player_id=1; SELECT COUNT(*) FROM keyless_accounts; SELECT COUNT(*) FROM keyless_bonuses;"
    if ($LASTEXITCODE -ne 0 -or ($keylessJoinDmlState -join "`n") -ne "15`t1`n16`t1`n27`t1`n14`n1`n1") {
        throw "keyless duplicate JOIN UPDATE/DELETE compatibility check failed"
    }
    $insertSetState = docker run --rm --network "${project}_default" mysql:8.0 `
        mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root `
        --batch --skip-column-names --execute="USE smoke; CREATE TABLE insert_set_rows (id BIGINT AUTO_INCREMENT PRIMARY KEY,actor VARCHAR(20) UNIQUE,value BIGINT DEFAULT 7,note VARCHAR(20)); INSERT INTO insert_set_rows SET actor='a',value=10,note=CONCAT('x','y'); INSERT INTO insert_set_rows SET actor='a',value=3 ON DUPLICATE KEY UPDATE value=value+VALUES(value),note='updated'; INSERT IGNORE INTO insert_set_rows SET actor='a',value=99; REPLACE INTO insert_set_rows SET actor='a',value=5,note='replace'; INSERT INTO insert_set_rows SET actor='b',value=DEFAULT; START TRANSACTION; INSERT INTO insert_set_rows SET actor='rollback'; ROLLBACK; SELECT id,actor,value,IFNULL(note,'NULL') FROM insert_set_rows ORDER BY id; SELECT COUNT(*) FROM insert_set_rows WHERE actor='rollback';"
    if ($LASTEXITCODE -ne 0 -or ($insertSetState -join "`n") -ne "4`ta`t5`treplace`n5`tb`t7`tNULL`n0") {
        throw "INSERT/REPLACE SET compatibility check failed"
    }
    $insertValuesState = docker run --rm --network "${project}_default" mysql:8.0 `
        mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root `
        --batch --skip-column-names --execute="USE smoke; CREATE TABLE insert_value_defaults (id BIGINT AUTO_INCREMENT PRIMARY KEY,actor VARCHAR(20) DEFAULT 'anon',value BIGINT DEFAULT 7,note VARCHAR(30) NULL); INSERT INTO insert_value_defaults () VALUES (); INSERT INTO insert_value_defaults VALUES (); INSERT INTO insert_value_defaults(actor,value,note) VALUES(DEFAULT,DEFAULT,CONCAT('x','y')); INSERT INTO insert_value_defaults(actor,value,note) VALUES(UPPER('bob'),1+2,IF(1=1,'yes','no')); INSERT INTO insert_value_defaults(id,actor,value,note) VALUES(DEFAULT(id),DEFAULT(actor),DEFAULT(value),CONCAT(DEFAULT(actor),'-',DEFAULT(value))); START TRANSACTION; INSERT INTO insert_value_defaults VALUES (); ROLLBACK; INSERT INTO insert_value_defaults VALUES (); SELECT id,actor,value,IFNULL(note,'NULL') FROM insert_value_defaults ORDER BY id; CREATE TABLE required_default_row (id BIGINT AUTO_INCREMENT PRIMARY KEY,required_value VARCHAR(10) NOT NULL);"
    if ($LASTEXITCODE -ne 0 -or ($insertValuesState -join "`n") -ne "1`tanon`t7`tNULL`n2`tanon`t7`tNULL`n3`tanon`t7`txy`n4`tBOB`t3`tyes`n5`tanon`t7`tanon-7`n7`tanon`t7`tNULL") {
        throw "INSERT VALUES default row/expression compatibility check failed"
    }
    $updateDefaultState = docker run --rm --network "${project}_default" mysql:8.0 `
        mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root `
        --batch --skip-column-names --execute="USE smoke; CREATE TABLE default_updates (id BIGINT AUTO_INCREMENT PRIMARY KEY,actor VARCHAR(20) UNIQUE,value BIGINT DEFAULT 7,note VARCHAR(30) DEFAULT 'base',optional VARCHAR(10) DEFAULT NULL); INSERT INTO default_updates(actor,value,note,optional) VALUES('a',10,'custom','x'); UPDATE default_updates SET value=DEFAULT,note=CONCAT(DEFAULT(note),'-u'),optional=DEFAULT(optional) WHERE actor='a'; SELECT value,note,IFNULL(optional,'NULL') FROM default_updates; INSERT INTO default_updates(actor,value,note) VALUES('a',99,'incoming') ON DUPLICATE KEY UPDATE value=DEFAULT,note=DEFAULT(note); SELECT value,note,IFNULL(optional,'NULL') FROM default_updates; CREATE TABLE keyless_defaults (value BIGINT DEFAULT 9,note VARCHAR(20) DEFAULT 'k'); INSERT INTO keyless_defaults VALUES(1,'x'),(2,'y'); UPDATE keyless_defaults SET value=DEFAULT,note=DEFAULT(note); SELECT value,note,COUNT(*) FROM keyless_defaults GROUP BY value,note; CREATE TABLE default_marker (id BIGINT PRIMARY KEY); INSERT INTO default_marker VALUES(1); UPDATE default_updates SET note='custom' WHERE id=1; UPDATE default_updates d JOIN default_marker m ON d.id=m.id SET d.note=DEFAULT(d.note); SELECT note FROM default_updates WHERE id=1; START TRANSACTION; UPDATE default_updates SET value=99 WHERE id=1; UPDATE default_updates SET value=DEFAULT WHERE id=1; ROLLBACK; SELECT value FROM default_updates WHERE id=1; CREATE TABLE required_update (id BIGINT PRIMARY KEY,required_value VARCHAR(10) NOT NULL); INSERT INTO required_update VALUES(1,'x');"
    if ($LASTEXITCODE -ne 0 -or ($updateDefaultState -join "`n") -ne "7`tbase-u`tNULL`n7`tbase`tNULL`n9`tk`t2`nbase`n7") {
        throw "UPDATE/UPSERT DEFAULT compatibility check failed"
    }
    $aliasedUpsertState = docker run --rm --network "${project}_default" mysql:8.0 `
        mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root `
        --batch --skip-column-names --execute="USE smoke; CREATE TABLE aliased_upserts (id BIGINT AUTO_INCREMENT PRIMARY KEY,actor VARCHAR(20) UNIQUE,value BIGINT DEFAULT 7,note VARCHAR(20)); INSERT INTO aliased_upserts(actor,value,note) VALUES('a',10,'base'); INSERT INTO aliased_upserts(actor,value,note) VALUES('a',3,'row') AS new ON DUPLICATE KEY UPDATE value=aliased_upserts.value+new.value,note=new.note; INSERT INTO aliased_upserts(actor,value,note) VALUES('a',2,'cols') AS new(a,v,n) ON DUPLICATE KEY UPDATE value=aliased_upserts.value+v,note=n; INSERT INTO aliased_upserts(actor,value,note) VALUES('a',1,'qualified') AS new(a,v,n) ON DUPLICATE KEY UPDATE value=aliased_upserts.value+new.v,note=new.n; INSERT INTO aliased_upserts SET actor='a',value=1,note='set' AS new ON DUPLICATE KEY UPDATE value=aliased_upserts.value+new.value,note=new.note; START TRANSACTION; INSERT INTO aliased_upserts(actor,value,note) VALUES('a',100,'rollback') AS new ON DUPLICATE KEY UPDATE value=aliased_upserts.value+new.value,note=new.note; ROLLBACK; INSERT INTO aliased_upserts(actor,value,note) VALUES('b',5,'insert') AS new ON DUPLICATE KEY UPDATE value=new.value,note=new.note; SELECT id,actor,value,note FROM aliased_upserts ORDER BY id;"
    if ($LASTEXITCODE -ne 0 -or ($aliasedUpsertState -join "`n") -ne "1`ta`t17`tset`n7`tb`t5`tinsert") {
        throw "MySQL 8 INSERT row/column alias UPSERT compatibility check failed"
    }
    $scalarUpsertState = docker run --rm --network "${project}_default" mysql:8.0 `
        mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root `
        --batch --skip-column-names --execute="USE smoke; CREATE TABLE scalar_upserts (id BIGINT AUTO_INCREMENT PRIMARY KEY,actor VARCHAR(20) UNIQUE,value BIGINT,note VARCHAR(100),state VARCHAR(20)); INSERT INTO scalar_upserts(actor,value,note,state) VALUES('a',10,'base','old'); INSERT INTO scalar_upserts(actor,value,note,state) VALUES('a',15,'incoming','x') AS new ON DUPLICATE KEY UPDATE value=GREATEST(scalar_upserts.value,new.value)+1,note=CONCAT(scalar_upserts.note,':',new.note),state=IF(new.value>10,'raised','kept'); INSERT INTO scalar_upserts(actor,value,note,state) VALUES('a',2,'next','x') AS new ON DUPLICATE KEY UPDATE value=scalar_upserts.value+new.value,note=CONCAT(scalar_upserts.note,'-',scalar_upserts.value),state=CASE WHEN scalar_upserts.value>=18 THEN 'high' ELSE 'low' END; INSERT INTO scalar_upserts(actor,value,note,state) VALUES('a',NULL,NULL,NULL) AS new ON DUPLICATE KEY UPDATE value=COALESCE(new.value,scalar_upserts.value),note=COALESCE(new.note,scalar_upserts.note),state=COALESCE(new.state,scalar_upserts.state); SELECT id,actor,value,note,state FROM scalar_upserts;"
    if ($LASTEXITCODE -ne 0 -or ($scalarUpsertState -join "`n") -ne "1`ta`t18`tbase:incoming-18`thigh") {
        throw "complex scalar UPSERT/left-to-right compatibility check failed"
    }
    docker run --rm --network "${project}_default" mysql:8.0 `
        mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root `
        --execute="USE smoke; CREATE TABLE affected_updates (id BIGINT PRIMARY KEY,value BIGINT,note VARCHAR(20)); INSERT INTO affected_updates VALUES(1,10,'x'),(2,20,'y'); CREATE TABLE affected_source (id BIGINT PRIMARY KEY,value BIGINT); INSERT INTO affected_source VALUES(1,11),(2,99); CREATE TABLE affected_keyless (value BIGINT,note VARCHAR(20)); INSERT INTO affected_keyless VALUES(1,'x'),(1,'x'); CREATE TABLE affected_upsert (id BIGINT AUTO_INCREMENT PRIMARY KEY,actor VARCHAR(20) UNIQUE,value BIGINT); INSERT INTO affected_upsert(actor,value) VALUES('a',1);"
    if ($LASTEXITCODE -ne 0) { throw "UPDATE affected-rows fixture setup failed" }
    $affectedNoop = docker exec mydb mydb-cli --password root --database smoke --execute="UPDATE affected_updates SET value=10 WHERE id=1"
    if ($LASTEXITCODE -ne 0 -or ($affectedNoop -join "`n").Trim() -ne "Query OK, 0 rows affected") {
        throw "literal no-op UPDATE affected rows mismatch"
    }
    $affectedChanged = docker exec mydb mydb-cli --password root --database smoke --execute="UPDATE affected_updates SET value=11 WHERE id=1"
    if ($LASTEXITCODE -ne 0 -or ($affectedChanged -join "`n").Trim() -ne "Query OK, 1 rows affected") {
        throw "changed UPDATE affected rows mismatch"
    }
    $affectedExpressionNoop = docker exec mydb mydb-cli --password root --database smoke --execute="UPDATE affected_updates SET value=value"
    if ($LASTEXITCODE -ne 0 -or ($affectedExpressionNoop -join "`n").Trim() -ne "Query OK, 0 rows affected") {
        throw "expression no-op UPDATE affected rows mismatch"
    }
    $affectedJoin = docker exec mydb mydb-cli --password root --database smoke --execute="UPDATE affected_updates a JOIN affected_source b ON a.id=b.id SET a.value=b.value"
    if ($LASTEXITCODE -ne 0 -or ($affectedJoin -join "`n").Trim() -ne "Query OK, 1 rows affected") {
        throw "JOIN UPDATE affected rows mismatch"
    }
    $affectedKeylessNoop = docker exec mydb mydb-cli --password root --database smoke --execute="UPDATE affected_keyless SET value=1,note='x'"
    if ($LASTEXITCODE -ne 0 -or ($affectedKeylessNoop -join "`n").Trim() -ne "Query OK, 0 rows affected") {
        throw "keyless no-op UPDATE affected rows mismatch"
    }
    $affectedKeylessChanged = docker exec mydb mydb-cli --password root --database smoke --execute="UPDATE affected_keyless SET value=2"
    if ($LASTEXITCODE -ne 0 -or ($affectedKeylessChanged -join "`n").Trim() -ne "Query OK, 2 rows affected") {
        throw "keyless changed UPDATE affected rows mismatch"
    }
    $affectedUpsertNoop = docker exec mydb mydb-cli --password root --database smoke --execute="INSERT INTO affected_upsert(actor,value) VALUES('a',1) AS new ON DUPLICATE KEY UPDATE value=new.value"
    if ($LASTEXITCODE -ne 0 -or ($affectedUpsertNoop -join "`n").Trim() -ne "Query OK, 0 rows affected") {
        throw "no-op UPSERT affected rows mismatch"
    }
    $createLikeState = docker run --rm --network "${project}_default" mysql:8.0 `
        mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root `
        --batch --skip-column-names --execute="USE smoke; CREATE TABLE like_parent (id BIGINT PRIMARY KEY); INSERT INTO like_parent VALUES(7); CREATE TABLE like_source (id BIGINT AUTO_INCREMENT PRIMARY KEY,actor VARCHAR(32) NOT NULL UNIQUE,score BIGINT DEFAULT 7,KEY score_idx(score),CONSTRAINT chk_score CHECK(score>=0),CONSTRAINT fk_score FOREIGN KEY(score) REFERENCES like_parent(id)) ENGINE=InnoDB AUTO_INCREMENT=42; INSERT INTO like_source(actor,score) VALUES('source',7); CREATE TABLE like_copy LIKE like_source; INSERT INTO like_copy(actor,score) VALUES('copy',99); SELECT id,actor,score FROM like_copy; SELECT COUNT(*) FROM like_copy; SHOW CREATE TABLE like_copy; CREATE TABLE IF NOT EXISTS like_copy LIKE like_source; SHOW COUNT(*) WARNINGS; SHOW WARNINGS;"
    $createLikeText = $createLikeState -join "`n"
    if ($LASTEXITCODE -ne 0 -or $createLikeText -notmatch "(?m)^1`tcopy`t99$" -or $createLikeText -notmatch "UNIQUE" -or $createLikeText -notmatch "score_idx" -or $createLikeText -notmatch "like_copy_chk_1" -or $createLikeText -match "FOREIGN KEY" -or $createLikeText -match "AUTO_INCREMENT=42" -or $createLikeText -notmatch "(?m)^Note`t1050`tTable 'like_copy' already exists$") {
        throw "CREATE TABLE LIKE metadata compatibility check failed"
    }
    # Windows PowerShell 5 turns redirected native stderr into terminating
    # NativeCommandError under Stop. Commands below intentionally fail; every
    # result is checked using exit status and expected MySQL error text.
    $ErrorActionPreference = "Continue"
    $insertValuesRequiredError = docker run --rm --network "${project}_default" mysql:8.0 `
        mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root `
        --batch --skip-column-names --execute="USE smoke; INSERT INTO required_default_row VALUES ();" 2>&1
    if ($LASTEXITCODE -eq 0 -or ($insertValuesRequiredError -join "`n") -notmatch "ERROR 1364 .*required_value.*doesn't have a default value") {
        throw "INSERT default row missing-field error compatibility check failed"
    }
    $updateRequiredError = docker run --rm --network "${project}_default" mysql:8.0 `
        mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root `
        --batch --skip-column-names --execute="USE smoke; UPDATE required_update SET required_value=DEFAULT WHERE id=1;" 2>&1
    if ($LASTEXITCODE -eq 0 -or ($updateRequiredError -join "`n") -notmatch "ERROR 1364 .*required_value.*doesn't have a default value") {
        throw "UPDATE DEFAULT missing-field error compatibility check failed"
    }
    $createLikeCheckError = docker run --rm --network "${project}_default" mysql:8.0 `
        mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root `
        --batch --skip-column-names --execute="USE smoke; INSERT INTO like_copy(actor,score) VALUES('bad',-1);" 2>&1
    if ($LASTEXITCODE -eq 0 -or ($createLikeCheckError -join "`n") -notmatch "ERROR 3819 .*like_copy_chk_1") {
        throw "CREATE TABLE LIKE CHECK constraint was not copied"
    }
    $truncateState = docker run --rm --network "${project}_default" mysql:8.0 `
        mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root `
        --batch --skip-column-names --execute="USE smoke; CREATE TABLE truncate_audit (id BIGINT PRIMARY KEY); CREATE TABLE truncate_auto (id BIGINT AUTO_INCREMENT PRIMARY KEY,value BIGINT); INSERT INTO truncate_auto(value) VALUES(10),(20); DELETE FROM truncate_auto; START TRANSACTION; INSERT INTO truncate_audit VALUES(1); TRUNCATE TABLE truncate_auto; ROLLBACK; INSERT INTO truncate_auto(value) VALUES(30); SELECT COUNT(*) FROM truncate_audit; SELECT COUNT(*) FROM truncate_auto; SELECT id FROM truncate_auto; CREATE TABLE truncate_parent (id BIGINT PRIMARY KEY); CREATE TABLE truncate_child (id BIGINT PRIMARY KEY,parent_id BIGINT,CONSTRAINT truncate_fk FOREIGN KEY(parent_id) REFERENCES truncate_parent(id)); INSERT INTO truncate_parent VALUES(1); INSERT INTO truncate_child VALUES(1,1);"
    if ($LASTEXITCODE -ne 0 -or ($truncateState -join "`n").Trim() -ne "1`n1`n1") {
        throw "TRUNCATE implicit commit/auto-increment reset check failed"
    }
    $truncateFkError = docker run --rm --network "${project}_default" mysql:8.0 `
        mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root `
        --batch --skip-column-names --execute="USE smoke; TRUNCATE TABLE truncate_parent;" 2>&1
    if ($LASTEXITCODE -eq 0 -or ($truncateFkError -join "`n") -notmatch "ERROR 1701 .*truncate_child.*truncate_fk") {
        throw "TRUNCATE foreign-key error compatibility check failed"
    }
    $truncateFkDisabledState = docker run --rm --network "${project}_default" mysql:8.0 `
        mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root `
        --batch --skip-column-names --execute="USE smoke; TRUNCATE TABLE truncate_child; SET FOREIGN_KEY_CHECKS=0; TRUNCATE TABLE truncate_parent; SET FOREIGN_KEY_CHECKS=1; SELECT COUNT(*) FROM truncate_child; SELECT COUNT(*) FROM truncate_parent;"
    if ($LASTEXITCODE -ne 0 -or ($truncateFkDisabledState -join "`n").Trim() -ne "0`n0") {
        throw "TRUNCATE child/FK-disabled compatibility check failed"
    }
    $loadDataScript = @'
printf 'id,name,note\n101,"loader","hello,world"\n102,bob,plain\n' >/tmp/mydb-load.csv
printf '301,\351,\351\n302,only\n303,a,b,extra\n301,dup,x\n' >/tmp/mydb-load-warnings.csv
mysql --local-infile=1 --protocol=TCP --host=mydb --port=3306 --user=root --password=root --batch --skip-column-names --execute="USE smoke; CREATE TABLE load_players (id BIGINT PRIMARY KEY,name VARCHAR(32),note VARCHAR(64)) ENGINE=InnoDB; LOAD DATA LOCAL INFILE '/tmp/mydb-load.csv' INTO TABLE load_players FIELDS TERMINATED BY ',' OPTIONALLY ENCLOSED BY '\"' LINES TERMINATED BY '\n' IGNORE 1 LINES (@raw_id,@raw_name,@raw_note) SET id=@raw_id,name=UPPER(@raw_name),note=UPPER(@raw_note); SELECT id,name,note FROM load_players ORDER BY id; CREATE TABLE load_warnings (id BIGINT PRIMARY KEY,name VARCHAR(32),raw BLOB) ENGINE=InnoDB; LOAD DATA LOCAL INFILE '/tmp/mydb-load-warnings.csv' INTO TABLE load_warnings CHARACTER SET latin1 FIELDS TERMINATED BY ','; SHOW COUNT(*) WARNINGS; SHOW WARNINGS; SELECT id,HEX(name),IFNULL(HEX(raw),'NULL') FROM load_warnings ORDER BY id;"
'@
    # Windows PowerShell 5.1 rewrites nested quotes in a multiline `sh -c` argument.
    # Feed source through stdin so MySQL SQL quoting reaches the client byte-for-byte.
    # Windows PowerShell 5.1 prepends a UTF-8 BOM to native stdin pipelines.
    # GNU sed removes it when present and leaves PowerShell 7 input unchanged.
    $loadDataState = $loadDataScript | docker run --rm -i --network "${project}_default" --entrypoint sh mysql:8.0 -c "sed '1s/^\xEF\xBB\xBF//' | tr -d '\r' | sh"
    $expectedLoadData = "101`tLOADER`tHELLO,WORLD`n102`tBOB`tPLAIN`n3`nWarning`t1261`tRow 2 doesn't contain data for all columns`nWarning`t1262`tRow 3 was truncated; it contained more data than there were input columns`nWarning`t1062`tDuplicate entry '301' for key 'load_warnings.PRIMARY'`n301`tC3A9`tE9`n302`t6F6E6C79`tNULL`n303`t61`t62"
    if ($LASTEXITCODE -ne 0 -or ($loadDataState -join "`n") -ne $expectedLoadData) {
        throw "LOAD DATA LOCAL INFILE protocol check failed"
    }
    docker exec mydb sh -c "printf '201\tserver\tsecure-file\n' >/var/lib/mydb/imports/server-load.tsv"
    if ($LASTEXITCODE -ne 0) { throw "cannot stage secure_file_priv fixture" }
    $serverLoadDataState = docker run --rm --network "${project}_default" mysql:8.0 `
        mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root `
        --batch --skip-column-names --execute="USE smoke; CREATE TABLE server_load_players (id BIGINT PRIMARY KEY,name VARCHAR(32),note VARCHAR(64)) ENGINE=InnoDB; LOAD DATA INFILE '/var/lib/mydb/imports/server-load.tsv' INTO TABLE server_load_players; SELECT id,name,note FROM server_load_players;"
    if ($LASTEXITCODE -ne 0 -or ($serverLoadDataState -join "`n") -ne "201`tserver`tsecure-file") {
        throw "secure_file_priv LOAD DATA INFILE check failed"
    }
    docker exec mydb sh -c "printf '401,a,b\n402,only\n403,c,d\n' >/var/lib/mydb/imports/strict-missing.csv; printf '411,a,b\n412,c,d,extra\n413,e,f\n' >/var/lib/mydb/imports/strict-extra.csv; printf '421,ok,a\n422,\377,b\n423,end,c\n' >/var/lib/mydb/imports/strict-invalid.csv"
    if ($LASTEXITCODE -ne 0) { throw "cannot stage strict LOAD DATA fixtures" }
    $strictMissingError = docker run --rm --network "${project}_default" mysql:8.0 `
        mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root `
        --batch --skip-column-names --execute="USE smoke; CREATE TABLE strict_missing (id BIGINT PRIMARY KEY,name VARCHAR(32),raw BLOB); LOAD DATA INFILE '/var/lib/mydb/imports/strict-missing.csv' INTO TABLE strict_missing FIELDS TERMINATED BY ',';" 2>&1
    if ($LASTEXITCODE -eq 0 -or ($strictMissingError -join "`n") -notmatch "ERROR 1261 .*Row 2 doesn't contain data for all columns") {
        throw "strict LOAD DATA missing-field error mismatch"
    }
    $strictExtraError = docker run --rm --network "${project}_default" mysql:8.0 `
        mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root `
        --batch --skip-column-names --execute="USE smoke; CREATE TABLE strict_extra (id BIGINT PRIMARY KEY,name VARCHAR(32),raw BLOB); LOAD DATA INFILE '/var/lib/mydb/imports/strict-extra.csv' INTO TABLE strict_extra FIELDS TERMINATED BY ',';" 2>&1
    if ($LASTEXITCODE -eq 0 -or ($strictExtraError -join "`n") -notmatch "ERROR 1262 .*Row 2 was truncated") {
        throw "strict LOAD DATA extra-field error mismatch"
    }
    $strictInvalidError = docker run --rm --network "${project}_default" mysql:8.0 `
        mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root `
        --batch --skip-column-names --execute="USE smoke; CREATE TABLE strict_invalid (id BIGINT PRIMARY KEY,name VARCHAR(32),raw BLOB); LOAD DATA INFILE '/var/lib/mydb/imports/strict-invalid.csv' IGNORE INTO TABLE strict_invalid CHARACTER SET utf8mb4 FIELDS TERMINATED BY ',';" 2>&1
    if ($LASTEXITCODE -eq 0 -or ($strictInvalidError -join "`n") -notmatch "ERROR 1300 .*Invalid utf8mb4 character string") {
        throw "strict LOAD DATA invalid-character error mismatch"
    }
    $strictLoadCounts = docker run --rm --network "${project}_default" mysql:8.0 `
        mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root `
        --batch --skip-column-names --execute="USE smoke; SELECT COUNT(*) FROM strict_missing; SELECT COUNT(*) FROM strict_extra; SELECT COUNT(*) FROM strict_invalid;"
    if ($LASTEXITCODE -ne 0 -or ($strictLoadCounts -join "`n") -ne "0`n0`n0") {
        throw "strict LOAD DATA was not statement-atomic"
    }
    $outsideLoadError = docker run --rm --network "${project}_default" mysql:8.0 `
        mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root `
        --batch --skip-column-names --execute="USE smoke; LOAD DATA INFILE '/etc/passwd' INTO TABLE server_load_players;" 2>&1
    if ($LASTEXITCODE -eq 0 -or ($outsideLoadError -join "`n") -notmatch "secure_file_priv") {
        throw "LOAD DATA escaped secure_file_priv"
    }
    $savepointExpressionState = docker run --rm --network "${project}_default" mysql:8.0 `
        mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root `
        --batch --skip-column-names --execute="USE smoke; START TRANSACTION; SAVEPOINT before_change; UPDATE players SET score=99 WHERE id=1; SAVEPOINT after_change; ROLLBACK TO before_change; RELEASE SAVEPOINT before_change; COMMIT; SELECT score FROM players WHERE id=1; SELECT CASE WHEN score>=20 THEN CONCAT(UPPER(name),'-',CAST(score AS CHAR)) ELSE CONCAT_WS(':',name,NULL,'rookie') END,ROUND(score/3,2),CAST(score/3 AS DECIMAL(10,1)),CONVERT(score,SIGNED) FROM players WHERE id=1; SELECT id FROM players WHERE CASE WHEN CONCAT(LOWER(name),'-',CAST(score AS CHAR))='alice-10' THEN 1 ELSE 0 END=1;"
    if ($LASTEXITCODE -ne 0 -or ($savepointExpressionState -join "`n") -ne "10`nalice:rookie`t3.33`t3.3`t10`n1") {
        throw "SAVEPOINT/CASE/string/numeric/CAST check failed"
    }
    $scalarMutationState = docker run --rm --network "${project}_default" mysql:8.0 `
        mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root `
        --batch --skip-column-names --execute="USE smoke; SELECT id FROM players ORDER BY score DESC,LOWER(name),id DESC; CREATE TABLE scalar_smoke (id BIGINT PRIMARY KEY,name VARCHAR(32),score BIGINT,state VARCHAR(16)); INSERT INTO scalar_smoke VALUES (1,'alice',10,'low'),(2,'bob',20,'low'); START TRANSACTION; UPDATE scalar_smoke SET score=CASE WHEN LOWER(name)='alice' THEN score+5 ELSE score END,name=CONCAT(UPPER(name),'-',CAST(score AS CHAR)),state=IF(score>=15,'high','low') WHERE CONCAT(LOWER(name),'-',id)='alice-1'; SELECT name,score,state FROM scalar_smoke WHERE id=1; ROLLBACK; SELECT name,score,state FROM scalar_smoke WHERE id=1; START TRANSACTION; DELETE FROM scalar_smoke WHERE CASE WHEN score>=20 THEN 1 ELSE 0 END=1 ORDER BY LOWER(name) LIMIT 1; SELECT COUNT(*) FROM scalar_smoke; ROLLBACK; SELECT COUNT(*) FROM scalar_smoke;"
    if ($LASTEXITCODE -ne 0 -or ($scalarMutationState -join "`n") -ne "4`n2`n1`nALICE-15`t15`thigh`nalice`t10`tlow`n1`n2") {
        throw "multi/expression ORDER BY and scalar UPDATE/DELETE check failed"
    }
    $setState = docker run --rm --network "${project}_default" mysql:8.0 `
        mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root `
        --batch --skip-column-names --execute="USE smoke; SELECT DISTINCT c.clan_name FROM players p CROSS JOIN clans c ORDER BY c.clan_name LIMIT 1 OFFSET 1; SELECT COUNT(DISTINCT actor_id) FROM players; SELECT c.clan_name,COUNT(*) AS n FROM players p CROSS JOIN clans c GROUP BY c.clan_name HAVING n > 2 ORDER BY c.clan_name; SELECT actor_id FROM players WHERE EXISTS (SELECT actor_id,clan_name FROM clans WHERE clan_name='red') ORDER BY id LIMIT 1;"
    if ($LASTEXITCODE -ne 0 -or ($setState -join "`n").Trim() -ne "red`n3`nblue`t3`nred`t3`na1") {
        throw "DISTINCT/OFFSET/HAVING/EXISTS check failed"
    }
    $ruWriter = Start-Job -ArgumentList $project -ScriptBlock {
        param($projectName)
        docker run --rm --network "${projectName}_default" mysql:8.0 `
            mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root `
            --batch --skip-column-names `
            --execute="START TRANSACTION; UPDATE smoke.ru_probe SET value=99 WHERE id=1; SELECT SLEEP(4); ROLLBACK;" `
            2>&1 | Out-Null
        if ($LASTEXITCODE -ne 0) { throw "RU writer failed" }
    }
    $ruDirty = @()
    $deadline = (Get-Date).AddSeconds(10)
    do {
        $ruDirty = docker run --rm --network "${project}_default" mysql:8.0 `
            mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root `
            --batch --skip-column-names `
            --execute="SET SESSION TRANSACTION ISOLATION LEVEL READ UNCOMMITTED; START TRANSACTION; SELECT value FROM smoke.ru_probe WHERE id=1; COMMIT;"
        if ($LASTEXITCODE -eq 0 -and ($ruDirty -join "`n").Trim() -eq "99") { break }
        Start-Sleep -Milliseconds 200
    } while ((Get-Date) -lt $deadline)
    if ($LASTEXITCODE -ne 0 -or ($ruDirty -join "`n").Trim() -ne "99") {
        throw "READ UNCOMMITTED dirty-read check failed"
    }
    Wait-Job $ruWriter | Out-Null
    Receive-Job $ruWriter | Out-Null
    Remove-Job $ruWriter
    $ruAfter = docker run --rm --network "${project}_default" mysql:8.0 `
        mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root `
        --batch --skip-column-names --execute="SELECT value FROM smoke.ru_probe WHERE id=1;"
    if ($LASTEXITCODE -ne 0 -or ($ruAfter -join "`n").Trim() -ne "10") {
        throw "READ UNCOMMITTED rollback cleanup check failed"
    }
    docker run --rm --network "${project}_default" mysql:8.0 `
        mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root `
        --execute="USE smoke; CREATE TABLE share_lock_probe (id BIGINT PRIMARY KEY,value BIGINT); INSERT INTO share_lock_probe VALUES(1,10),(2,20);"
    if ($LASTEXITCODE -ne 0) { throw "FOR SHARE fixture setup failed" }
    $shareLocker = Start-Job -ArgumentList $project -ScriptBlock {
        param($projectName)
        docker run --rm --network "${projectName}_default" mysql:8.0 `
            mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root `
            --batch --skip-column-names `
            --execute="USE smoke; START TRANSACTION; SELECT value FROM share_lock_probe WHERE id=1 FOR SHARE; SELECT SLEEP(4); COMMIT;" `
            2>&1 | Out-Null
        if ($LASTEXITCODE -ne 0) { throw "FOR SHARE locker failed" }
    }
    Start-Sleep -Seconds 1
    $shareNowait = docker run --rm --network "${project}_default" mysql:8.0 `
        mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root `
        --batch --skip-column-names `
        --execute="USE smoke; START TRANSACTION; SELECT value FROM share_lock_probe WHERE id=1 FOR SHARE NOWAIT; COMMIT;"
    if ($LASTEXITCODE -ne 0 -or ($shareNowait -join "`n").Trim() -ne "10") {
        throw "FOR SHARE NOWAIT compatibility check failed"
    }
    $updateNowaitError = docker run --rm --network "${project}_default" mysql:8.0 `
        mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root `
        --batch --skip-column-names `
        --execute="USE smoke; START TRANSACTION; SELECT value FROM share_lock_probe WHERE id=1 FOR UPDATE NOWAIT; COMMIT;" 2>&1
    if ($LASTEXITCODE -eq 0 -or ($updateNowaitError -join "`n") -notmatch "ERROR 3572 .*NOWAIT is set") {
        throw "FOR UPDATE NOWAIT error compatibility check failed"
    }
    $skipUpdate = docker run --rm --network "${project}_default" mysql:8.0 `
        mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root `
        --batch --skip-column-names `
        --execute="USE smoke; START TRANSACTION; SELECT id FROM share_lock_probe ORDER BY id LIMIT 1 FOR UPDATE SKIP LOCKED; COMMIT;"
    if ($LASTEXITCODE -ne 0 -or ($skipUpdate -join "`n").Trim() -ne "2") {
        throw "FOR UPDATE SKIP LOCKED ordering/LIMIT check failed"
    }
    $skipShare = docker run --rm --network "${project}_default" mysql:8.0 `
        mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root `
        --batch --skip-column-names `
        --execute="USE smoke; START TRANSACTION; SELECT id FROM share_lock_probe ORDER BY id LIMIT 2 FOR SHARE SKIP LOCKED; COMMIT;"
    if ($LASTEXITCODE -ne 0 -or ($skipShare -join "`n").Trim() -ne "1`n2") {
        throw "FOR SHARE SKIP LOCKED compatibility check failed"
    }
    $unlockedRow = docker run --rm --network "${project}_default" mysql:8.0 `
        mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root `
        --batch --skip-column-names `
        --execute="USE smoke; UPDATE share_lock_probe SET value=value+1 WHERE id=2; SELECT value FROM share_lock_probe WHERE id=2;"
    if ($LASTEXITCODE -ne 0 -or ($unlockedRow -join "`n").Trim() -ne "21") {
        throw "FOR SHARE locked an unrelated primary-key row"
    }
    Wait-Job $shareLocker | Out-Null
    Receive-Job $shareLocker | Out-Null
    Remove-Job $shareLocker
    docker run --rm --network "${project}_default" mysql:8.0 `
        mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root `
        --execute="USE smoke; CREATE TABLE deadlock_probe (id BIGINT PRIMARY KEY,value BIGINT); INSERT INTO deadlock_probe VALUES(1,1),(2,2);"
    if ($LASTEXITCODE -ne 0) { throw "deadlock fixture setup failed" }
    $deadlockFirst = Start-Job -ArgumentList $project -ScriptBlock {
        param($projectName)
        $output = docker run --rm --network "${projectName}_default" mysql:8.0 `
            mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root `
            --batch --skip-column-names `
            --execute="USE smoke; START TRANSACTION; UPDATE deadlock_probe SET value=value+10 WHERE id=1; SELECT SLEEP(3); UPDATE deadlock_probe SET value=value+10 WHERE id=2; COMMIT;" 2>&1
        [pscustomobject]@{ ExitCode = $LASTEXITCODE; Output = ($output -join "`n") }
    }
    Start-Sleep -Seconds 1
    $deadlockSecond = Start-Job -ArgumentList $project -ScriptBlock {
        param($projectName)
        $output = docker run --rm --network "${projectName}_default" mysql:8.0 `
            mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root `
            --batch --skip-column-names `
            --execute="USE smoke; START TRANSACTION; UPDATE deadlock_probe SET value=value+100 WHERE id=2; SELECT SLEEP(1); UPDATE deadlock_probe SET value=value+100 WHERE id=1; COMMIT;" 2>&1
        [pscustomobject]@{ ExitCode = $LASTEXITCODE; Output = ($output -join "`n") }
    }
    Wait-Job $deadlockFirst, $deadlockSecond | Out-Null
    $deadlockFirstResult = Receive-Job $deadlockFirst
    $deadlockSecondResult = Receive-Job $deadlockSecond
    Remove-Job $deadlockFirst, $deadlockSecond
    if ($deadlockFirstResult.ExitCode -eq 0 -or $deadlockFirstResult.Output -notmatch "ERROR 1213 .*Deadlock found when trying to get lock" -or $deadlockSecondResult.ExitCode -ne 0) {
        throw "deadlock victim/survivor compatibility check failed"
    }
    $deadlockState = docker run --rm --network "${project}_default" mysql:8.0 `
        mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root `
        --batch --skip-column-names --execute="USE smoke; SELECT id,value FROM deadlock_probe ORDER BY id;"
    if ($LASTEXITCODE -ne 0 -or ($deadlockState -join "`n").Trim() -ne "1`t101`n2`t102") {
        throw "deadlock victim transaction was not fully rolled back"
    }
    $deadlockMetrics = Invoke-WebRequest -UseBasicParsing -Uri "http://127.0.0.1:14316/metrics"
    if ($deadlockMetrics.Content -notmatch "mydb_deadlocks_total [1-9]") {
        throw "deadlock metric was not incremented"
    }

    docker run --rm --network "${project}_default" mysql:8.0 `
        mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root `
        --execute="USE smoke; CREATE TABLE crash_probe (id BIGINT PRIMARY KEY,value BIGINT); INSERT INTO crash_probe VALUES(1,10);"
    if ($LASTEXITCODE -ne 0) { throw "crash recovery fixture setup failed" }
    $crashWriter = Start-Job -ArgumentList $project -ScriptBlock {
        param($projectName)
        $output = docker run --rm --network "${projectName}_default" mysql:8.0 `
            mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root `
            --batch --skip-column-names `
            --execute="USE smoke; START TRANSACTION; UPDATE crash_probe SET value=99 WHERE id=1; SELECT SLEEP(30); COMMIT;" 2>&1
        [pscustomobject]@{ ExitCode = $LASTEXITCODE; Output = ($output -join "`n") }
    }
    Start-Sleep -Seconds 1
    $crashDirty = docker run --rm --network "${project}_default" mysql:8.0 `
        mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root `
        --batch --skip-column-names `
        --execute="SET SESSION TRANSACTION ISOLATION LEVEL READ UNCOMMITTED; START TRANSACTION; SELECT value FROM smoke.crash_probe WHERE id=1; COMMIT;"
    if ($LASTEXITCODE -ne 0 -or ($crashDirty -join "`n").Trim() -ne "99") {
        throw "crash writer did not reach uncommitted dirty state"
    }
    docker kill --signal=KILL mydb | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "SIGKILL injection failed" }
    Wait-Job $crashWriter | Out-Null
    $crashWriterResult = Receive-Job $crashWriter
    Remove-Job $crashWriter
    if ($crashWriterResult.ExitCode -eq 0) {
        throw "SIGKILL did not interrupt the active transaction client"
    }
    Start-Sleep -Seconds 1
    $containerState = docker inspect --format '{{.State.Status}}' mydb 2>$null
    if ($containerState -ne "running") {
        docker start mydb | Out-Null
        if ($LASTEXITCODE -ne 0) { throw "container did not restart after SIGKILL" }
    }
    $health = ""
    $deadline = (Get-Date).AddMinutes(2)
    do {
        $health = docker inspect --format '{{.State.Health.Status}}' mydb 2>$null
        if ($health -eq "healthy") { break }
        Start-Sleep -Seconds 2
    } while ((Get-Date) -lt $deadline)
    if ($health -ne "healthy") { throw "MyDB container did not recover after SIGKILL" }
    $crashState = docker run --rm --network "${project}_default" mysql:8.0 `
        mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root `
        --batch --skip-column-names `
        --execute="USE smoke; SELECT value FROM crash_probe WHERE id=1; START TRANSACTION; UPDATE crash_probe SET value=value+1 WHERE id=1; COMMIT; SELECT value FROM crash_probe WHERE id=1;"
    if ($LASTEXITCODE -ne 0 -or ($crashState -join "`n").Trim() -ne "10`n11") {
        throw "SIGKILL recovery lost committed data or retained uncommitted data"
    }
    docker stop --time 30 mydb | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "cannot stop container for WAL torn-tail injection" }
    $walInjectionScript = @'
wal=$(ls -1 /var/lib/mydb/data/wal/wal_*.log | sort | tail -n 1)
before=$(wc -c < "$wal")
printf "\001\002\003\004\005" >> "$wal"
after=$(wc -c < "$wal")
printf "%s\t%s\t%s\n" "$wal" "$before" "$after"
'@
    $walInjection = $walInjectionScript | docker run --rm -i --volumes-from mydb --entrypoint sh mydb:dev -c "sed '1s/^\xEF\xBB\xBF//' | tr -d '\r' | sh"
    if ($LASTEXITCODE -ne 0) { throw "WAL torn-tail injection failed" }
    $walParts = ($walInjection -join "").Trim().Split("`t")
    if ($walParts.Count -ne 3) { throw "WAL injection metadata is invalid" }
    $walPath = $walParts[0]
    $walValidLength = [long]$walParts[1]
    $walInjectedLength = [long]$walParts[2]
    if ($walInjectedLength -ne $walValidLength + 5) {
        throw "WAL torn-tail bytes were not appended"
    }
    docker start mydb | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "cannot restart after WAL torn-tail injection" }
    $health = ""
    $deadline = (Get-Date).AddMinutes(2)
    do {
        $health = docker inspect --format '{{.State.Health.Status}}' mydb 2>$null
        if ($health -eq "healthy") { break }
        Start-Sleep -Seconds 2
    } while ((Get-Date) -lt $deadline)
    if ($health -ne "healthy") { throw "MyDB did not recover the WAL torn tail" }
    $walRecoveredLength = docker run --rm --volumes-from mydb --entrypoint wc mydb:dev -c $walPath
    $walRecoveredBytes = (($walRecoveredLength -join "").Trim() -split '\s+')[0]
    if ($LASTEXITCODE -ne 0 -or [long]$walRecoveredBytes -ne $walValidLength) {
        throw "WAL torn tail was not truncated to the last valid record"
    }
    $walRecoveryState = docker run --rm --network "${project}_default" mysql:8.0 `
        mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root `
        --batch --skip-column-names `
        --execute="USE smoke; SELECT value FROM crash_probe WHERE id=1; UPDATE crash_probe SET value=value+1 WHERE id=1; SELECT value FROM crash_probe WHERE id=1;"
    if ($LASTEXITCODE -ne 0 -or ($walRecoveryState -join "`n").Trim() -ne "11`n12") {
        throw "database state is invalid after WAL torn-tail recovery"
    }
    $persistedState = docker run --rm --network "${project}_default" mysql:8.0 `
        mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root `
        --batch --skip-column-names --execute="USE smoke; SELECT COUNT(*) FROM players; SELECT name,score FROM players WHERE actor_id='a2';"
    if ($LASTEXITCODE -ne 0 -or ($persistedState -join "`n") -ne "3`nbob2`t21") {
        throw "persistence/UPSERT state check failed"
    }
    $memoryState = docker run --rm --network "${project}_default" mysql:8.0 `
        mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root `
        --batch --skip-column-names --execute="USE smoke; SELECT COUNT(*) FROM volatile_sessions; SELECT COUNT(*) FROM information_schema.tables WHERE table_schema=DATABASE() AND table_name='volatile_sessions'; SHOW CREATE TABLE volatile_sessions;"
    if ($LASTEXITCODE -ne 0 -or ($memoryState -join "`n") -notmatch "(?m)^0`n1`nvolatile_sessions`t.*ENGINE=MEMORY") {
        throw "MEMORY restart semantics check failed"
    }
    $schemaRows = docker run --rm --network "${project}_default" mysql:8.0 `
        mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root `
        --batch --skip-column-names --execute="USE smoke; SELECT COUNT(*) FROM information_schema.tables WHERE table_schema=DATABASE() AND table_name='players';"
    if ($LASTEXITCODE -ne 0 -or $schemaRows.Trim() -ne "1") { throw "information_schema check failed" }
    docker run --rm --network "${project}_default" mysql:8.0 `
        mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root `
        --batch --skip-column-names --execute="USE smoke; UPDATE players SET score=10 WHERE id=1; UPDATE players SET score=score+0.5,score=score*2 WHERE id=1; INSERT INTO players (id,actor_id,name,score) VALUES (1,'a1','ignored',3) ON DUPLICATE KEY UPDATE score=score+VALUES(score); SELECT score FROM players WHERE id=1;" |
        Tee-Object -Variable actorCounterState | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "row-lock update check failed" }
    if (($actorCounterState -join "`n").Trim() -ne "25") {
        throw "atomic actor counter UPDATE check failed"
    }
    $fkState = docker run --rm --network "${project}_default" mysql:8.0 `
        mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root `
        --batch --skip-column-names --execute="USE smoke; CREATE TABLE fk_accounts (id BIGINT PRIMARY KEY); CREATE TABLE fk_items (id BIGINT PRIMARY KEY, account_id BIGINT, amount BIGINT, payload BLOB, CONSTRAINT fk_item_account FOREIGN KEY (account_id) REFERENCES fk_accounts (id) ON DELETE CASCADE ON UPDATE RESTRICT, CONSTRAINT chk_item_amount CHECK (amount > 0)); INSERT INTO fk_accounts VALUES (1); INSERT INTO fk_items VALUES (10,1,2,0x00FF5C27); SELECT HEX(payload) FROM fk_items WHERE id=10;"
    if ($LASTEXITCODE -ne 0 -or $fkState.Trim() -ne "00FF5C27") {
        throw "foreign key/CHECK/0x binary setup check failed"
    }
    $fkError = docker run --rm --network "${project}_default" mysql:8.0 `
        mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root `
        --execute="USE smoke; INSERT INTO fk_items VALUES (11,999,1,NULL);" 2>&1
    if ($LASTEXITCODE -eq 0 -or ($fkError -join "`n") -notmatch "ERROR 1452") {
        throw "foreign key rejection/error-code check failed"
    }
    $checkError = docker run --rm --network "${project}_default" mysql:8.0 `
        mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root `
        --execute="USE smoke; INSERT INTO fk_items VALUES (12,1,0,NULL);" 2>&1
    if ($LASTEXITCODE -eq 0 -or ($checkError -join "`n") -notmatch "ERROR 3819") {
        throw "CHECK constraint rejection/error-code check failed"
    }
    $ErrorActionPreference = "Stop"
    $fkSwitch = docker run --rm --network "${project}_default" mysql:8.0 `
        mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root `
        --batch --skip-column-names --execute="SET @OLD_FOREIGN_KEY_CHECKS=@@FOREIGN_KEY_CHECKS, FOREIGN_KEY_CHECKS=0; SELECT @@FOREIGN_KEY_CHECKS; SET FOREIGN_KEY_CHECKS=@OLD_FOREIGN_KEY_CHECKS; SELECT @@FOREIGN_KEY_CHECKS;"
    if ($LASTEXITCODE -ne 0 -or ($fkSwitch -join "`n") -ne "0`n1") {
        throw "FOREIGN_KEY_CHECKS session switch failed"
    }
    $fkCascade = docker run --rm --network "${project}_default" mysql:8.0 `
        mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root `
        --batch --skip-column-names --execute="USE smoke; START TRANSACTION; DELETE FROM fk_accounts WHERE id=1; SELECT COUNT(*) FROM fk_items; ROLLBACK; SELECT COUNT(*) FROM fk_items; DELETE FROM fk_accounts WHERE id=1; SELECT COUNT(*) FROM fk_items; INSERT INTO fk_accounts VALUES (2); INSERT INTO fk_items VALUES (20,2,3,0xDEADBEEF); START TRANSACTION; SET FOREIGN_KEY_CHECKS=0; INSERT INTO fk_items VALUES (30,999,1,NULL); SET FOREIGN_KEY_CHECKS=1; INSERT INTO fk_accounts VALUES (3); INSERT INTO fk_items VALUES (31,3,1,NULL); COMMIT; SELECT COUNT(*) FROM fk_items WHERE id IN (30,31);"
    if ($LASTEXITCODE -ne 0 -or ($fkCascade -join "`n").Trim() -ne "0`n1`n0`n2") {
        throw "transactional foreign key cascade/rollback/session-switch check failed"
    }
    docker run --rm --network "${project}_default" mysql:8.0 `
        mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root `
        --batch --skip-column-names --execute="USE smoke; CREATE TABLE mutation_queue (id BIGINT PRIMARY KEY, value BIGINT); INSERT INTO mutation_queue VALUES (1,1),(2,2),(3,3),(4,4); UPDATE mutation_queue SET value=value+10 ORDER BY id DESC LIMIT 2; DELETE FROM mutation_queue ORDER BY value ASC LIMIT 1; UPDATE mutation_queue SET id=id+10 ORDER BY id DESC; SELECT id,value FROM mutation_queue ORDER BY id; CREATE TABLE duplicate_events (value BIGINT, note VARCHAR(10)); INSERT INTO duplicate_events VALUES (1,'a'),(1,'a'),(1,'a'); UPDATE duplicate_events SET value=9 WHERE value=1 LIMIT 1; DELETE FROM duplicate_events WHERE value=1 LIMIT 1; SELECT value,COUNT(*) FROM duplicate_events GROUP BY value ORDER BY value;" |
        Tee-Object -Variable orderedMutationState | Out-Null
    if ($LASTEXITCODE -ne 0 -or ($orderedMutationState -join "`n").Trim() -ne "12`t2`n13`t13`n14`t14`n1`t1`n9`t1") {
        throw "ordered/limited UPDATE DELETE check failed"
    }

    $metrics = Invoke-WebRequest -UseBasicParsing -Uri "http://127.0.0.1:14316/metrics"
    if ($metrics.Content -notmatch "mydb_up 1" -or
        $metrics.Content -notmatch "mydb_row_lock_acquires_total [1-9]" -or
        $metrics.Content -notmatch "mydb_wal_sync_microseconds_total [1-9]" -or
        $metrics.Content -notmatch "mydb_checkpoint_errors_total 0") {
        throw "Prometheus check failed"
    }
    $headers = @{ Authorization = "Bearer root" }
    $agent = Invoke-RestMethod -Headers $headers -Uri "http://127.0.0.1:14316/api/v1/agent/health"
    if ($null -eq $agent.healthy -or $null -eq $agent.active_transaction_locks -or
        $null -eq $agent.checkpoint_errors) { throw "Agent HTTP check failed" }
    $agentAsk = $null
    for ($attempt = 0; $attempt -lt 3; $attempt++) {
        $agentAsk = Invoke-RestMethod -Method Post -Headers $headers -ContentType "application/json; charset=utf-8" `
            -Body ([Text.Encoding]::UTF8.GetBytes('{"question":"为什么写入延迟高？"}')) `
            -Uri "http://127.0.0.1:14316/api/v1/agent/ask"
        if ([string]$agentAsk.intent -eq "write_latency") { break }
        Start-Sleep -Milliseconds 200
    }
    if ([string]$agentAsk.intent -ne "write_latency") { throw "Agent natural-language intent check failed" }

    $fullBackup = Invoke-RestMethod -Method Post -Headers $headers `
        -Uri "http://127.0.0.1:14316/api/v1/backup/full"
    docker run --rm --network "${project}_default" mysql:8.0 `
        mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root `
        --batch --skip-column-names --execute="USE smoke; INSERT INTO players (actor_id,name,score) VALUES ('a4','delta',40);"
    if ($LASTEXITCODE -ne 0) { throw "incremental seed write failed" }
    $pointInTime = [DateTime]::UtcNow.ToString("o")
    Start-Sleep -Milliseconds 200
    docker run --rm --network "${project}_default" mysql:8.0 `
        mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root `
        --batch --skip-column-names --execute="USE smoke; INSERT INTO players (actor_id,name,score) VALUES ('a5','after_target',50);"
    if ($LASTEXITCODE -ne 0) { throw "post-backup write failed" }
    $incrementalBody = @{ base_id = $fullBackup.id } | ConvertTo-Json -Compress
    $incrementalBackup = Invoke-RestMethod -Method Post -Headers $headers -ContentType "application/json" `
        -Body $incrementalBody -Uri "http://127.0.0.1:14316/api/v1/backup/incremental"
    $restoreBody = @{ id = $incrementalBackup.id; point_in_time = $pointInTime } | ConvertTo-Json -Compress
    $restore = Invoke-RestMethod -Method Post -Headers $headers -ContentType "application/json" `
        -Body $restoreBody -Uri "http://127.0.0.1:14316/api/v1/backup/restore"
    if (-not $restore.restart_required) { throw "backup restore was not staged safely" }
    docker compose -p $project restart mydb
    if ($LASTEXITCODE -ne 0) { throw "backup restore restart failed" }
    $health = ""
    $deadline = (Get-Date).AddMinutes(2)
    do {
        $health = docker inspect --format '{{.State.Health.Status}}' mydb 2>$null
        if ($health -eq "healthy") { break }
        Start-Sleep -Seconds 2
    } while ((Get-Date) -lt $deadline)
    if ($health -ne "healthy") { throw "MyDB did not recover the backup chain" }
    $backupState = docker run --rm --network "${project}_default" mysql:8.0 `
        mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root `
        --batch --skip-column-names --execute="USE smoke; SELECT COUNT(*) FROM players WHERE actor_id='a4'; SELECT COUNT(*) FROM players WHERE actor_id='a5'; SELECT account_id,amount,HEX(payload) FROM fk_items WHERE id=20;"
    if ($LASTEXITCODE -ne 0 -or ($backupState -join "`n") -ne "1`n0`n2`t3`tDEADBEEF") {
        throw "full + LSN incremental PITR chain check failed"
    }
    docker stop --time 30 mydb | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "cannot stop container for read-only data-dir injection" }
    $readOnlyDataScript = @'
set -eu
chmod -R a-w /var/lib/mydb/data
'@
    $readOnlyDataScript | docker run --rm -i --user root --volumes-from mydb --entrypoint sh mydb:dev -c "sed '1s/^\xEF\xBB\xBF//' | tr -d '\r' | sh"
    if ($LASTEXITCODE -ne 0) { throw "cannot make data directory read-only" }
    docker start mydb | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "cannot restart with read-only data directory" }
    Start-Sleep -Seconds 3
    $readOnlyHealth = docker inspect --format '{{.State.Health.Status}}' mydb 2>$null
    $previousErrorAction = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    try {
        $readOnlyLog = docker logs mydb 2>&1
    } finally {
        $ErrorActionPreference = $previousErrorAction
    }
    if ($readOnlyHealth -eq "healthy" -or ($readOnlyLog -join "`n") -notmatch "Permission denied|os error 13") {
        throw "read-only data directory did not make startup fail safely"
    }
    docker stop --time 1 mydb | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "cannot stop read-only failure container" }
    $writableDataScript = @'
set -eu
chmod -R u+rwX /var/lib/mydb/data
'@
    $writableDataScript | docker run --rm -i --user root --volumes-from mydb --entrypoint sh mydb:dev -c "sed '1s/^\xEF\xBB\xBF//' | tr -d '\r' | sh"
    if ($LASTEXITCODE -ne 0) { throw "cannot restore data directory write permissions" }
    docker start mydb | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "cannot restart after read-only data-dir recovery" }
    $health = ""
    $deadline = (Get-Date).AddMinutes(2)
    do {
        $health = docker inspect --format '{{.State.Health.Status}}' mydb 2>$null
        if ($health -eq "healthy") { break }
        Start-Sleep -Seconds 2
    } while ((Get-Date) -lt $deadline)
    if ($health -ne "healthy") { throw "MyDB did not recover after read-only data-dir failure" }
    $readOnlyRecoveryState = docker run --rm --network "${project}_default" mysql:8.0 `
        mysql --protocol=TCP --host=mydb --port=3306 --user=root --password=root `
        --batch --skip-column-names --execute="USE smoke; UPDATE crash_probe SET value=value+1 WHERE id=1; SELECT value FROM crash_probe WHERE id=1;"
    if ($LASTEXITCODE -ne 0 -or [string]::IsNullOrWhiteSpace(($readOnlyRecoveryState -join "`n").Trim())) {
        throw "read-only data-dir recovery could not commit a new WAL write"
    }
    $enospcContainer = "mydb-enospc-$PID"
    try {
        docker run --detach --name $enospcContainer --network "${project}_default" `
            --tmpfs /var/lib/mydb:rw,size=8m,mode=1777 `
            -e MYDB_ROOT_PASSWORD=root -e MYDB_ADMIN_PASSWORD=root `
            -e MYDB_ENFORCE_STRONG_PASSWORDS=false mydb:dev | Out-Null
        if ($LASTEXITCODE -ne 0) { throw "cannot start ENOSPC MyDB fixture" }
        $enospcReady = $false
        $deadline = (Get-Date).AddMinutes(1)
        do {
            $enospcState = docker inspect --format '{{.State.Status}}' $enospcContainer 2>$null
            if ($enospcState -eq "running") {
                docker exec $enospcContainer mydb-server --healthcheck 2>$null | Out-Null
                $enospcReady = $LASTEXITCODE -eq 0
            }
            if (-not $enospcReady) { Start-Sleep -Seconds 1 }
        } while (-not $enospcReady -and (Get-Date) -lt $deadline)
        if (-not $enospcReady) { throw "ENOSPC MyDB fixture did not become healthy" }
        $previousErrorAction = $ErrorActionPreference
        $ErrorActionPreference = "Continue"
        try {
            $enospcError = docker run --rm --network "${project}_default" mysql:8.0 `
                mysql --protocol=TCP --host=$enospcContainer --port=3306 --user=root --password=root `
                --execute="CREATE DATABASE smoke; USE smoke; CREATE TABLE blobs (id BIGINT PRIMARY KEY,payload LONGBLOB); INSERT INTO blobs VALUES (1,REPEAT('x',12000000));" 2>&1
        } finally {
            $ErrorActionPreference = $previousErrorAction
        }
        if ($LASTEXITCODE -eq 0 -or ($enospcError -join "`n") -notmatch "ERROR 1105 .*No space left on device") {
            throw "ENOSPC write did not return a safe MySQL error"
        }
        $enospcState = docker inspect --format '{{.State.Status}}' $enospcContainer 2>$null
        $enospcRows = docker run --rm --network "${project}_default" mysql:8.0 `
            mysql --protocol=TCP --host=$enospcContainer --port=3306 --user=root --password=root `
            --batch --skip-column-names --execute="USE smoke; SELECT COUNT(*) FROM blobs;"
        if ($enospcState -ne "running" -or $LASTEXITCODE -ne 0 -or ($enospcRows -join "`n").Trim() -ne "0") {
            throw "ENOSPC write crashed the server or left a partial row"
        }
    } finally {
        $previousErrorAction = $ErrorActionPreference
        $ErrorActionPreference = "Continue"
        docker rm --force $enospcContainer 2>&1 | Out-Null
        $ErrorActionPreference = $previousErrorAction
    }
    $pageCorruptionContainer = "mydb-page-corrupt"
    $pageCorruptionVolume = "${project}-page-corrupt-data"
    docker volume create $pageCorruptionVolume | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "cannot create page-corruption data volume" }
    try {
        docker run --detach --name $pageCorruptionContainer --network "${project}_default" `
            --mount "type=volume,src=$pageCorruptionVolume,dst=/var/lib/mydb" `
            mydb:dev | Out-Null
        if ($LASTEXITCODE -ne 0) { throw "cannot start page-corruption MyDB fixture" }
        $pageCorruptionReady = $false
        $deadline = (Get-Date).AddMinutes(2)
        do {
            $pageCorruptionHealth = ((docker inspect --format '{{.State.Health.Status}}' $pageCorruptionContainer 2>$null) -join "").Trim()
            if ($pageCorruptionHealth -eq "healthy") {
                $pageCorruptionReady = $true
                break
            }
            $pageCorruptionStatus = ((docker inspect --format '{{.State.Status}}' $pageCorruptionContainer 2>$null) -join "").Trim()
            if ($pageCorruptionStatus -eq "exited") { break }
            Start-Sleep -Seconds 2
        } while ((Get-Date) -lt $deadline)
        if (-not $pageCorruptionReady) { throw "page-corruption MyDB fixture did not become healthy" }
        docker run --rm --network "${project}_default" mysql:8.0 `
            mysql --protocol=TCP --host=$pageCorruptionContainer --port=3306 --user=root --password=root `
            --execute="CREATE DATABASE smoke; USE smoke; CREATE TABLE corrupt_pages (id BIGINT PRIMARY KEY,value BIGINT); INSERT INTO corrupt_pages VALUES (1,10);" | Out-Null
        if ($LASTEXITCODE -ne 0) { throw "page-corruption fixture setup failed" }
        docker stop --time 30 $pageCorruptionContainer | Out-Null
        if ($LASTEXITCODE -ne 0) { throw "cannot stop page-corruption fixture" }
        $pageCorruptionInjection = @'
set -eu
page=/var/lib/mydb/data/smoke/corrupt_pages/pages.dat
test -s "$page"
printf '\377' | dd of="$page" bs=1 seek=25 conv=notrunc status=none
'@
        $pageCorruptionInjection | docker run --rm -i `
            --mount "type=volume,src=$pageCorruptionVolume,dst=/var/lib/mydb" `
            --entrypoint sh mydb:dev -c "sed '1s/^\xEF\xBB\xBF//' | tr -d '\r' | sh"
        if ($LASTEXITCODE -ne 0) { throw "page-corruption injection failed" }
        docker start $pageCorruptionContainer | Out-Null
        if ($LASTEXITCODE -ne 0) { throw "cannot restart page-corruption fixture" }
        Start-Sleep -Seconds 3
        $pageCorruptionHealth = ((docker inspect --format '{{.State.Health.Status}}' $pageCorruptionContainer 2>$null) -join "").Trim()
        $previousErrorAction = $ErrorActionPreference
        $ErrorActionPreference = "Continue"
        try {
            $pageCorruptionLog = docker logs $pageCorruptionContainer 2>&1
        } finally {
            $ErrorActionPreference = $previousErrorAction
        }
        if ($pageCorruptionHealth -eq "healthy" -or ($pageCorruptionLog -join "`n") -notmatch "Page corruption") {
            throw "page corruption did not make startup fail safely"
        }
    } finally {
        $previousErrorAction = $ErrorActionPreference
        $ErrorActionPreference = "Continue"
        docker rm --force $pageCorruptionContainer 2>&1 | Out-Null
        docker volume rm --force $pageCorruptionVolume 2>&1 | Out-Null
        $ErrorActionPreference = $previousErrorAction
    }
    docker stop --time 30 mydb | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "cannot stop container for WAL middle-corruption injection" }
    $walMiddleCorruptionScript = @'
set -eu
wal=$(ls -1 /var/lib/mydb/data/wal/wal_*.log | sort | head -n 1)
test -n "$wal"
test "$(wc -c < "$wal")" -gt 20
printf '\377' | dd of="$wal" bs=1 seek=20 conv=notrunc status=none
'@
    $walMiddleCorruptionScript | docker run --rm -i --volumes-from mydb --entrypoint sh mydb:dev -c "sed '1s/^\xEF\xBB\xBF//' | tr -d '\r' | sh"
    if ($LASTEXITCODE -ne 0) { throw "WAL middle-corruption injection failed" }
    docker start mydb | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "cannot restart after WAL middle-corruption injection" }
    Start-Sleep -Seconds 3
    $corruptionHealth = docker inspect --format '{{.State.Health.Status}}' mydb 2>$null
    $previousErrorAction = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    try {
        $corruptionLog = docker logs mydb 2>&1
    } finally {
        $ErrorActionPreference = $previousErrorAction
    }
    if ($corruptionHealth -eq "healthy" -or ($corruptionLog -join "`n") -notmatch "WAL (CRC mismatch|corruption)") {
        throw "WAL middle corruption did not make startup fail safely"
    }
    Write-Host "Docker smoke passed: official/native MySQL CLI, changed-row affected counts/no-op WAL avoidance, SIGKILL committed/uncommitted, WAL torn-tail recovery, read-only data-dir refusal/recovery, ENOSPC atomic rejection and middle-corruption refusal, INSERT/REPLACE SET, INSERT VALUES default rows/expressions/1364, UPDATE/UPSERT/JOIN DEFAULT, MySQL 8 row/column alias and complex scalar/left-to-right UPSERT, CREATE TABLE LIKE, TRUNCATE implicit commit/auto-ID/FK 1701, FOR SHARE/NOWAIT 3572/SKIP LOCKED/deadlock 1213 rollback, LOAD DATA LOCAL/secure INFILE charset/warnings/strict atomic errors, transaction/SAVEPOINT/RU dirty read, MEMORY restart, advanced JOIN+aggregate+correlated/derived/CTE subquery, UNION/INTERSECT/EXCEPT, window/common scalar expressions, keyed/keyless multi-target JOIN UPDATE/DELETE, multi/expression ORDER BY, scalar and atomic actor UPDATE/DELETE/UPSERT, FK/CHECK/hex BLOB, row locks, auto-ID, predicates, schema, persistence, metrics, Agent NL/HTTP, full+LSN incremental PITR"
} finally {
    if (-not $Keep) {
        # Windows PowerShell may wrap native stderr progress as an ErrorRecord.
        # Cleanup output is non-fatal; preserve the real smoke-test result.
        $previousErrorAction = $ErrorActionPreference
        $ErrorActionPreference = "Continue"
        docker compose -p $project down -v --remove-orphans 2>&1 | Out-Null
        $ErrorActionPreference = $previousErrorAction
    }
}
