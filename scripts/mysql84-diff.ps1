#Requires -Version 5.1
[CmdletBinding()]
param(
    [string]$MyDbHost = "host.docker.internal",
    [int]$MyDbPort = 3306,
    [string]$MyDbUser = "root",
    [string]$MyDbPassword = "root"
)

$ErrorActionPreference = "Stop"
$container = "mydb-mysql-84-diff"
$database = "mydb_diff84_$([guid]::NewGuid().ToString('N'))"

function Invoke-MySqlClient {
    param(
        [ValidateSet("mysql84", "mydb")]
        [string]$Target,
        [string]$Sql
    )

    $arguments = @(
        "exec",
        "-e", "MYSQL_PWD=$($MyDbPassword)",
        $container,
        "mysql",
        "--protocol=TCP",
        "--batch",
        "--raw",
        "--column-names",
        "--connect-timeout=5",
        "-u$MyDbUser"
    )
    if ($Target -eq "mysql84") {
        $arguments += @("-e", $Sql)
    } else {
        $arguments += @("--host=$MyDbHost", "--port=$MyDbPort", "-e", $Sql)
    }

    $output = & docker @arguments 2>&1
    [pscustomobject]@{
        ExitCode = $LASTEXITCODE
        Output = ($output -join [Environment]::NewLine).Trim()
    }
}

function Normalize-Result {
    param([string]$Value)
    if ([string]::IsNullOrWhiteSpace($Value)) {
        return ""
    }
    return (($Value -replace "`r`n", "`n") -replace "`r", "`n").Trim()
}

function ConvertTo-ResultLines {
    param([string]$Value)
    return (Normalize-Result $Value).Split("`n")
}

function Get-JsonArrayText {
    param($Value)
    if ($null -eq $Value) {
        return ""
    }
    return (@($Value) | ForEach-Object { [string]$_ }) -join ","
}

function Compare-ExplainResult {
    param(
        [string]$MysqlText,
        [string]$MyDbText
    )
    $mysqlLines = ConvertTo-ResultLines $MysqlText
    $mydbLines = ConvertTo-ResultLines $MyDbText
    $mysqlJsonLine = [Array]::IndexOf($mysqlLines, "EXPLAIN")
    $mydbJsonLine = [Array]::IndexOf($mydbLines, "EXPLAIN")
    if ($mysqlJsonLine -lt 1 -or $mydbJsonLine -lt 1) {
        return $false
    }
    if (($mysqlLines[0..($mysqlJsonLine - 1)] -join "`n") -ne ($mydbLines[0..($mydbJsonLine - 1)] -join "`n")) {
        return $false
    }
    try {
        $mysqlJson = ($mysqlLines[($mysqlJsonLine + 1)..($mysqlLines.Count - 1)] -join "`n") | ConvertFrom-Json
        $mydbJson = ($mydbLines[($mydbJsonLine + 1)..($mydbLines.Count - 1)] -join "`n") | ConvertFrom-Json
    } catch {
        return $false
    }
    $mysqlTable = $mysqlJson.query_block.table
    $mydbTable = $mydbJson.query_block.table
    foreach ($field in @("table_name", "access_type", "key", "key_length", "filtered", "using_index")) {
        if ([string]$mysqlTable.$field -ne [string]$mydbTable.$field) {
            return $false
        }
    }
    foreach ($field in @("possible_keys", "used_key_parts", "ref", "used_columns")) {
        if ((Get-JsonArrayText $mysqlTable.$field) -ne (Get-JsonArrayText $mydbTable.$field)) {
            return $false
        }
    }
    return $true
}

function Compare-StatusResult {
    param(
        [string]$MysqlText,
        [string]$MyDbText
    )
    $mysqlLines = ConvertTo-ResultLines $MysqlText
    $mydbLines = ConvertTo-ResultLines $MyDbText
    $header = "Name`tEngine`tVersion`tRow_format`tRows`tAvg_row_length`tData_length`tMax_data_length`tIndex_length`tData_free`tAuto_increment`tCreate_time`tUpdate_time`tCheck_time`tCollation`tChecksum`tCreate_options`tComment"
    $mysqlHeader = [Array]::IndexOf($mysqlLines, $header)
    $mydbHeader = [Array]::IndexOf($mydbLines, $header)
    if ($mysqlHeader -lt 0 -or $mydbHeader -lt 0) {
        return $false
    }
    if ([string]::IsNullOrWhiteSpace($mysqlLines[$mysqlHeader + 1]) -or
        [string]::IsNullOrWhiteSpace($mydbLines[$mydbHeader + 1])) {
        return $false
    }
    return $true
}

function Compare-CheckConstraintResult {
    param(
        [string]$MysqlText,
        [string]$MyDbText
    )
    if ($MysqlText -notmatch "ERROR 3819" -or $MyDbText -notmatch "ERROR 3819") {
        return $false
    }
    $mysqlNormalized = (($MysqlText -split "`r?`n" | Where-Object {
            $_ -notmatch "^ERROR 3819" -and $_ -ne ""
        }) -join "`n")
    $mydbNormalized = (($MyDbText -split "`r?`n" | Where-Object {
            $_ -notmatch "^ERROR 3819" -and $_ -ne ""
        }) -join "`n")
    return (Normalize-Result $mysqlNormalized) -eq (Normalize-Result $mydbNormalized)
}

function Assert-Success {
    param(
        [string]$Target,
        [string]$Label,
        [pscustomobject]$Result
    )
    if ($Result.ExitCode -ne 0) {
        throw "$Target $Label failed (exit=$($Result.ExitCode)): $($Result.Output)"
    }
}

function Invoke-DiffCase {
    param(
        [string]$Name,
        [string]$Sql
    )
    $mysql = Invoke-MySqlClient -Target mysql84 -Sql $Sql
    $mydb = Invoke-MySqlClient -Target mydb -Sql $Sql
    $mysqlText = Normalize-Result $mysql.Output
    $mydbText = Normalize-Result $mydb.Output
    $sameOutput = if ($Name -eq "explain") {
        Compare-ExplainResult $mysqlText $mydbText
    } elseif ($Name -eq "check constraint") {
        Compare-CheckConstraintResult $mysqlText $mydbText
    } elseif ($Name -eq "show status and table status") {
        Compare-StatusResult $mysqlText $mydbText
    } else {
        $mysqlText -eq $mydbText
    }
    if ($mysql.ExitCode -ne $mydb.ExitCode -or -not $sameOutput) {
        Write-Host "[DIFF] $Name" -ForegroundColor Red
        Write-Host "SQL: $Sql"
        Write-Host "--- mysql:8.4 (exit=$($mysql.ExitCode))" -ForegroundColor Yellow
        Write-Host $mysqlText
        Write-Host "--- mydb (exit=$($mydb.ExitCode))" -ForegroundColor Yellow
        Write-Host $mydbText
        return $false
    }
    Write-Host "[OK] $Name" -ForegroundColor Green
    return $true
}

try {
    docker rm -f $container 2>$null | Out-Null
    # Match the Windows single-node deployment: MySQL lower_case_table_names=1.
    docker run --name $container -e MYSQL_ROOT_PASSWORD=root -d mysql:8.4 --lower-case-table-names=1 | Out-Null

    $deadline = [DateTime]::UtcNow.AddSeconds(90)
    do {
        $ping = docker exec $container mysqladmin ping --protocol=TCP --host=127.0.0.1 -uroot -proot 2>$null
        if ($ping -match "alive") {
            break
        }
        Start-Sleep -Seconds 2
    } while ([DateTime]::UtcNow -lt $deadline)
    if ($ping -notmatch "alive") {
        throw "mysql:8.4 container did not become ready"
    }

    $setup = "DROP DATABASE IF EXISTS $database; CREATE DATABASE $database; CREATE TABLE $database.accounts (id INT PRIMARY KEY, name VARCHAR(32) NOT NULL, amount DECIMAL(10,2), KEY ix_name(name)); INSERT INTO $database.accounts VALUES (1,'alpha',12.50),(2,'beta',0.00);"
    foreach ($target in @("mysql84", "mydb")) {
        $result = Invoke-MySqlClient -Target $target -Sql $setup
        Assert-Success -Target $target -Label "setup" -Result $result
    }

    $cases = @(
        [pscustomobject]@{ Name = "scalar expressions"; Sql = "SELECT 1 AS one, 1+2 AS sum, X'35'+4 AS hex_number, B'1010'+1 AS bit_number" },
        [pscustomobject]@{ Name = "null and conditional expressions"; Sql = "SELECT NULL IS NULL AS null_true, NULL <=> NULL AS null_safe, COALESCE(NULL,'fallback') AS coalesce_value, IF(2>1,'yes','no') AS if_value, NULLIF(1,1) AS nullif_value" },
        [pscustomobject]@{ Name = "boolean literals"; Sql = "SELECT TRUE AS true_value, FALSE AS false_value, NOT TRUE AS not_true, TRUE AND FALSE AS and_value" },
        [pscustomobject]@{ Name = "string expressions"; Sql = "SELECT CONCAT('a','b') AS concat_value, LOWER('AbC') AS lower_value, UPPER('aBc') AS upper_value, TRIM('  x  ') AS trim_value, SUBSTRING('abcdef',2,3) AS substring_value, REPLACE('abc','b','x') AS replace_value, CHAR_LENGTH(CONVERT(0xE4B8ADE69687 USING utf8mb4)) AS char_length, LENGTH(CONVERT(0xE4B8ADE69687 USING utf8mb4)) AS byte_length" },
        [pscustomobject]@{ Name = "temporal expressions"; Sql = "SELECT DATE_FORMAT('2024-02-29 12:34:56.123456','%Y-%m-%d %H:%i:%s.%f') AS formatted, DAYOFWEEK('2024-01-01') AS day_of_week, TIMESTAMPDIFF(DAY,'2024-01-01','2024-01-31') AS day_diff" },
        [pscustomobject]@{ Name = "json expressions"; Sql = "SELECT JSON_EXTRACT('{`"a`":[1,2]}','$.a[1]') AS extracted, JSON_UNQUOTE('`"hello`"') AS unquoted, JSON_TYPE('{`"a`":1}') AS json_type, JSON_LENGTH('[1,2,3]') AS json_length" },
        [pscustomobject]@{ Name = "json advanced expressions"; Sql = "SELECT JSON_ARRAY_APPEND('{`"a`":[1,2],`"s`":`"x`"}','$.a',3,'$.s','y') AS array_append, JSON_ARRAY_INSERT('{`"a`":[1,2]}','$.a[0]',0,'$.a[5]',5) AS array_insert, JSON_MERGE_PATCH('{`"a`":1,`"b`":{`"x`":2,`"y`":3}}','{`"a`":null,`"b`":{`"x`":9},`"c`":4}') AS merge_patch, JSON_DEPTH('[1,{`"a`" : [2]}]') AS depth, JSON_KEYS('{`"b`":1,`"a`":2}') AS json_keys, JSON_PRETTY('{`"a`":[1,{`"b`":true}],`"c`":null}') AS pretty" },
        [pscustomobject]@{ Name = "json search"; Sql = "SELECT JSON_SEARCH('{`"a`":`"foo`",`"b`":[`"bar`",`"foo`"]}','one','foo') AS search_one, JSON_SEARCH('{`"a`":`"foo`",`"b`":[`"bar`",`"foo`"]}','all','foo') AS search_all, JSON_SEARCH('{`"a`":`"foo`",`"b`":[`"bar`",`"foo`"]}','all','fo%') AS search_wildcard, JSON_SEARCH('{`"a`":`"foo`",`"b`":[`"bar`",`"foo`"]}','all','foo','x','$.b') AS search_path, JSON_SEARCH('{`"a.b`":`"foo`",`"plain`":`"foo`"}','all','foo') AS search_special" },
        [pscustomobject]@{ Name = "json overlap and path"; Sql = "SELECT JSON_OVERLAPS('[1,2]','[2,3]') AS overlaps, JSON_OVERLAPS('{`"a`":1}','{`"a`":1,`"b`":2}') AS object_overlap, JSON_CONTAINS_PATH('{`"a`":1}','one','$.a','$.missing') AS path_one, JSON_CONTAINS_PATH('{`"a`":1}','all','$.a','$.missing') AS path_all" },
        [pscustomobject]@{ Name = "row data"; Sql = "SELECT id,name,amount FROM $database.accounts ORDER BY id" },
        [pscustomobject]@{ Name = "case insensitive table identifiers"; Sql = "CREATE TABLE $database.CaseProbe (id INT PRIMARY KEY, value VARCHAR(16)); INSERT INTO $database.caseprobe VALUES (1,'ok'); SELECT id,value FROM $database.CASEPROBE" },
        [pscustomobject]@{ Name = "table and column comments"; Sql = "CREATE TABLE $database.comment_probe (id INT PRIMARY KEY COMMENT 'identifier', value VARCHAR(16) COMMENT 'payload') COMMENT='table-v1'; ALTER TABLE $database.comment_probe COMMENT='table-v2'; SELECT TABLE_COMMENT FROM information_schema.TABLES WHERE TABLE_SCHEMA='$database' AND TABLE_NAME='comment_probe'; SELECT COLUMN_NAME,COLUMN_COMMENT FROM information_schema.COLUMNS WHERE TABLE_SCHEMA='$database' AND TABLE_NAME='comment_probe' ORDER BY ORDINAL_POSITION" },
        [pscustomobject]@{ Name = "aggregate data"; Sql = "SELECT COUNT(*) AS row_count, SUM(amount) AS amount_sum, AVG(amount) AS amount_avg, MIN(name) AS name_min, MAX(name) AS name_max FROM $database.accounts" },
        [pscustomobject]@{ Name = "constant aggregate projection"; Sql = "SELECT DATABASE() AS database_name, COUNT(*) AS row_count, MAX(amount) AS amount_max FROM $database.accounts" },
        [pscustomobject]@{ Name = "group and having"; Sql = "SELECT name,COUNT(*) AS row_count,SUM(amount) AS amount_sum FROM $database.accounts GROUP BY name HAVING COUNT(*) > 0 ORDER BY name" },
        [pscustomobject]@{ Name = "window row number"; Sql = "SELECT id,ROW_NUMBER() OVER (ORDER BY id) AS rn FROM $database.accounts ORDER BY id" },
        [pscustomobject]@{ Name = "predicates"; Sql = "SELECT 1=1 AS equal_true, NULL<=>NULL AS null_safe, 3 BETWEEN 1 AND 4 AS between_true, 'abc' LIKE 'a%' AS like_true, 2 IN (1,2) AS in_true, NOT (1=0) AS not_true" },
        [pscustomobject]@{ Name = "casts and conversions"; Sql = "SELECT CAST('12.50' AS DECIMAL(10,2)) AS decimal_value, CAST(12 AS CHAR) AS char_value, CAST(1 AS UNSIGNED) AS unsigned_value, CONVERT('2024-01-02' USING utf8mb4) AS converted_value" },
        [pscustomobject]@{ Name = "numeric functions"; Sql = "SELECT ABS(-3) AS abs_value, ROUND(12.345,2) AS rounded_value, FLOOR(2.9) AS floor_value, CEIL(2.1) AS ceil_value, MOD(7,3) AS mod_value, POWER(2,3) AS power_value" },
        [pscustomobject]@{ Name = "binary functions"; Sql = "SELECT HEX(UNHEX('4142')) AS hex_value, TO_BASE64('abc') AS base64_value, FROM_BASE64('YWJj') AS decoded_value, BIT_COUNT(7) AS bit_count" },
        [pscustomobject]@{ Name = "bit aggregates"; Sql = "SELECT BIT_AND(amount) AS bit_and_value, BIT_OR(amount) AS bit_or_value, BIT_XOR(amount) AS bit_xor_value FROM $database.accounts" },
        [pscustomobject]@{ Name = "json mutation"; Sql = "SELECT JSON_OBJECT('id',1,'name','alpha') AS object_value, JSON_ARRAY(1,'a',NULL) AS array_value, JSON_CONTAINS('{`"a`":1}','1','$.a') AS contains_value, JSON_SET('{`"a`":1}','$.b',2) AS set_value, JSON_REMOVE('{`"a`":1,`"b`":2}','$.b') AS remove_value" },
        [pscustomobject]@{ Name = "date functions"; Sql = "SELECT DATE('2024-02-29 12:34:56') AS date_value, TIME('2024-02-29 12:34:56') AS time_value, DATEDIFF('2024-02-29','2024-02-01') AS date_diff, DATE_ADD('2024-01-31',INTERVAL 1 MONTH) AS month_end, LAST_DAY('2024-02-10') AS last_day" },
        [pscustomobject]@{ Name = "time zone session"; Sql = "SET time_zone='+00:00'; SELECT CONVERT_TZ('2024-01-01 12:00:00','+00:00','+08:00') AS converted_time, @@session.time_zone AS session_zone; SET time_zone='SYSTEM'" },
        [pscustomobject]@{ Name = "compatibility system variables"; Sql = "SELECT @@default_tmp_storage_engine AS default_tmp_storage_engine, @@authentication_policy AS authentication_policy" },
        [pscustomobject]@{ Name = "kill unknown thread errors"; Sql = "KILL CONNECTION 999999; KILL QUERY 999999" },
        [pscustomobject]@{ Name = "datagrip connection probes"; Sql = "SELECT DATABASE() AS database_name; SELECT @@event_scheduler AS event_scheduler; SELECT @@default_storage_engine AS default_storage_engine,@@default_tmp_storage_engine AS default_tmp_storage_engine; SELECT @@authentication_policy AS policy; SELECT @@GLOBAL.lower_case_table_names AS lower_case_table_names" },
        [pscustomobject]@{ Name = "datagrip schema list"; Sql = "SELECT schema_name,default_collation_name FROM information_schema.schemata WHERE schema_name='$database'" },
        [pscustomobject]@{ Name = "datagrip tables and views"; Sql = "SELECT T.table_name AS table_name,T.table_type AS table_type,T.table_comment AS table_comment,T.engine AS engine,T.table_collation AS table_collation,T.create_options AS create_options,V.definer AS view_definer FROM information_schema.tables T LEFT JOIN information_schema.views V ON T.table_schema=V.table_schema AND T.table_name=V.table_name WHERE T.table_schema='$database' AND true ORDER BY T.table_name" },
        [pscustomobject]@{ Name = "datagrip columns"; Sql = "SELECT ordinal_position,column_name,column_type,column_default,generation_expression,table_name,column_comment,is_nullable,extra,collation_name FROM information_schema.columns WHERE table_schema='$database' AND true ORDER BY table_name,ordinal_position" },
        [pscustomobject]@{ Name = "datagrip major names"; Sql = "SELECT table_schema AS schema_name,table_name AS major_name,CASE WHEN table_type LIKE '%TABLE' THEN 'T' WHEN table_type LIKE '%VIEW' THEN 'V' WHEN table_type IN ('TEMPORARY','SYSTEM VERSIONED') THEN 'T' END AS major_kind,CAST(NULL AS CHAR(1)) AS routine_kind FROM information_schema.tables WHERE table_schema='$database' UNION ALL SELECT routine_schema AS schema_name,routine_name AS major_name,'R' AS major_kind,CASE WHEN ROUTINE_TYPE LIKE 'P%' THEN 'P' WHEN ROUTINE_TYPE LIKE 'F%' THEN 'F' END AS routine_kind FROM information_schema.routines WHERE routine_schema='$database' UNION ALL SELECT event_schema AS schema_name,event_name AS major_name,'E' AS major_kind,CAST(NULL AS CHAR(1)) AS routine_kind FROM information_schema.events WHERE event_schema='$database' ORDER BY 1,2" },
        [pscustomobject]@{ Name = "datagrip minor names"; Sql = "SELECT T.table_schema AS schema_name,T.table_name AS major_name,CASE WHEN T.table_type LIKE '%TABLE' THEN 'T' WHEN T.table_type LIKE '%VIEW' THEN 'V' WHEN T.table_type IN ('TEMPORARY','SYSTEM VERSIONED') THEN 'T' END AS major_kind,C.ordinal_position AS position,CAST(NULL AS CHAR(1)) AS direction,C.column_name AS minor_name FROM information_schema.tables T,information_schema.columns C WHERE T.table_schema='$database' AND T.table_schema=C.table_schema AND T.table_name=C.table_name ORDER BY 1,2,4 LIMIT 10" },
        [pscustomobject]@{ Name = "distinct and ordering"; Sql = "SELECT DISTINCT name FROM $database.accounts ORDER BY name DESC" },
        [pscustomobject]@{ Name = "derived table"; Sql = "SELECT d.name,d.amount FROM (SELECT name,amount FROM $database.accounts WHERE amount > 0) AS d ORDER BY d.name" },
        [pscustomobject]@{ Name = "cte"; Sql = "WITH positive AS (SELECT id,name FROM $database.accounts WHERE amount > 0) SELECT id,name FROM positive ORDER BY id" },
        [pscustomobject]@{ Name = "scalar subquery"; Sql = "SELECT name,amount FROM $database.accounts WHERE amount=(SELECT MAX(amount) FROM $database.accounts) ORDER BY id" },
        [pscustomobject]@{ Name = "exists subquery"; Sql = "SELECT a.id FROM $database.accounts a WHERE EXISTS (SELECT 1 FROM $database.accounts b WHERE b.id=a.id AND b.amount>=0) ORDER BY a.id" },
        [pscustomobject]@{ Name = "self join"; Sql = "SELECT a.id,b.name FROM $database.accounts a JOIN $database.accounts b ON a.id=b.id WHERE a.amount>=0 ORDER BY a.id" },
        [pscustomobject]@{ Name = "window aggregate"; Sql = "SELECT id,amount,SUM(amount) OVER (ORDER BY id ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) AS running_sum FROM $database.accounts ORDER BY id" },
        [pscustomobject]@{ Name = "group concat"; Sql = "SELECT GROUP_CONCAT(name ORDER BY id SEPARATOR '|') AS names FROM $database.accounts" },
        [pscustomobject]@{ Name = "null aggregate"; Sql = "SELECT COUNT(amount) AS non_null_count, SUM(amount) AS amount_sum, AVG(amount) AS amount_avg FROM $database.accounts WHERE id>100" },
        [pscustomobject]@{ Name = "conditional aggregation"; Sql = "SELECT SUM(IF(amount>0,1,0)) AS positive_count, SUM(CASE WHEN amount=0 THEN 1 ELSE 0 END) AS zero_count FROM $database.accounts" },
        [pscustomobject]@{ Name = "sql prepared statement"; Sql = "SET @sql='SELECT ? + ? AS prepared_sum'; PREPARE stmt FROM @sql; SET @p1=2; SET @p2=3; EXECUTE stmt USING @p1,@p2; DEALLOCATE PREPARE stmt" },
        [pscustomobject]@{ Name = "metadata privileges"; Sql = "SELECT TABLE_SCHEMA,TABLE_NAME,PRIVILEGE_TYPE FROM information_schema.TABLE_PRIVILEGES WHERE TABLE_SCHEMA='$database' ORDER BY TABLE_NAME,PRIVILEGE_TYPE LIMIT 5" },
        [pscustomobject]@{ Name = "metadata boolean filter"; Sql = "SELECT COUNT(*) AS table_count FROM information_schema.TABLES WHERE TABLE_SCHEMA='$database' AND true; SELECT COUNT(*) AS column_count FROM information_schema.COLUMNS WHERE TABLE_SCHEMA='$database' AND false" },
        [pscustomobject]@{ Name = "metadata comma join"; Sql = "SELECT T.table_schema,C.table_name,C.ordinal_position FROM information_schema.tables T, information_schema.columns C WHERE T.table_schema='$database' AND T.table_schema=C.table_schema AND T.table_name=C.table_name ORDER BY 1,2,3 LIMIT 5" },
        [pscustomobject]@{ Name = "datagrip auto increments"; Sql = "SELECT table_name,auto_increment FROM information_schema.tables WHERE table_schema='$database' AND auto_increment IS NOT NULL AND true ORDER BY table_name" },
        [pscustomobject]@{ Name = "datagrip indices"; Sql = "SELECT table_name,index_name,index_comment,index_type,non_unique,column_name,sub_part,collation,expression FROM information_schema.statistics WHERE table_schema='$database' AND index_schema='$database' AND true ORDER BY index_schema,table_name,index_name,index_type,seq_in_index" },
        [pscustomobject]@{ Name = "datagrip constraints"; Sql = "SELECT c.constraint_name,c.constraint_schema,c.table_name,c.constraint_type,c.enforced='YES' AS enforced FROM information_schema.table_constraints c WHERE c.table_schema='$database' AND true ORDER BY c.table_name,c.constraint_name" },
        [pscustomobject]@{ Name = "datagrip constraint columns"; Sql = "SELECT constraint_name,table_name,column_name,referenced_table_schema,referenced_table_name,referenced_column_name FROM information_schema.key_column_usage WHERE table_schema='$database' AND referenced_column_name IS NOT NULL AND true ORDER BY table_name,constraint_name,ordinal_position" },
        [pscustomobject]@{ Name = "datagrip subpartitions"; Sql = "SELECT table_name,partition_name,subpartition_name,partition_ordinal_position,subpartition_ordinal_position,partition_method,subpartition_method,partition_expression,subpartition_expression,partition_description,partition_comment FROM information_schema.partitions WHERE partition_name IS NOT NULL AND table_schema='$database' AND true" },
        [pscustomobject]@{ Name = "datagrip triggers"; Sql = "SELECT trigger_name,event_object_table,event_manipulation,action_timing,definer FROM information_schema.triggers WHERE trigger_schema='$database' AND true ORDER BY trigger_name" },
        [pscustomobject]@{ Name = "datagrip events"; Sql = "SELECT event_name,event_comment,definer,event_type='RECURRING' AS recurring,interval_value,interval_field,CAST(COALESCE(starts,execute_at) AS CHAR) AS starts,CAST(ends AS CHAR) AS ends,status,on_completion='PRESERVE' AS preserve,last_executed FROM information_schema.events WHERE event_schema='$database' AND true ORDER BY event_name" },
        [pscustomobject]@{ Name = "datagrip routine grants"; Sql = "SELECT Host,User,Routine_name,Proc_priv,Routine_type='PROCEDURE' AS is_proc FROM mysql.procs_priv WHERE Db='$database' ORDER BY Host,User,Routine_name" },
        [pscustomobject]@{ Name = "datagrip privileges union"; Sql = "SELECT grantee,table_name,column_name,privilege_type,is_grantable FROM information_schema.column_privileges WHERE table_schema='$database' UNION ALL SELECT grantee,table_name,'' AS column_name,privilege_type,is_grantable FROM information_schema.table_privileges WHERE table_schema='$database' ORDER BY table_name,grantee,privilege_type" },
        [pscustomobject]@{ Name = "datagrip views"; Sql = "SELECT table_name,view_definition FROM information_schema.views WHERE table_schema='$database' AND true ORDER BY table_name" },
        [pscustomobject]@{ Name = "mysql user boolean filter"; Sql = "SELECT COUNT(*) > 0 AS has_root FROM mysql.user WHERE User='root' AND true" },
        [pscustomobject]@{ Name = "enum set binary temporal types"; Sql = "CREATE TABLE $database.type_roundtrip (id INT PRIMARY KEY, enum_value ENUM('new','used') NOT NULL, set_value SET('read','write','admin'), dec_value DECIMAL(10,3), tm TIME(3), dt DATETIME(6), binary_value BINARY(3), varbinary_value VARBINARY(8), bit_value BIT(4)); INSERT INTO $database.type_roundtrip VALUES (1,'used','read,admin',12.345,'12:34:56.789','2024-02-29 12:34:56.123456',0x414200,0x00FF, b'1010'); SELECT id,enum_value,set_value,dec_value,tm,dt,HEX(binary_value),HEX(varbinary_value),bit_value+0 FROM $database.type_roundtrip" },
        [pscustomobject]@{ Name = "alter rename truncate auto increment"; Sql = "USE $database; CREATE TABLE $database.ddl_probe (id INT AUTO_INCREMENT PRIMARY KEY, value VARCHAR(16) DEFAULT 'd'); INSERT INTO $database.ddl_probe (value) VALUES ('a'),('b'); ALTER TABLE $database.ddl_probe ADD COLUMN extra INT NOT NULL DEFAULT 7, ADD INDEX idx_extra (extra); ALTER TABLE $database.ddl_probe RENAME TO ddl_probe_renamed; SELECT id,value,extra FROM $database.ddl_probe_renamed ORDER BY id; TRUNCATE TABLE $database.ddl_probe_renamed; INSERT INTO $database.ddl_probe_renamed (value) VALUES ('after'); SELECT id,value,extra FROM $database.ddl_probe_renamed" },
        [pscustomobject]@{ Name = "temporary table lifecycle"; Sql = "CREATE TEMPORARY TABLE $database.temp_probe (id INT PRIMARY KEY, value VARCHAR(16)); INSERT INTO $database.temp_probe VALUES (1,'temporary'); SELECT TABLE_NAME FROM information_schema.TABLES WHERE TABLE_SCHEMA='$database' AND TABLE_NAME='temp_probe'; SELECT id,value FROM $database.temp_probe; DROP TEMPORARY TABLE $database.temp_probe" },
        [pscustomobject]@{ Name = "generated column surface"; Sql = "CREATE TABLE $database.generated_probe (id INT PRIMARY KEY, left_value INT, right_value INT, total_value INT GENERATED ALWAYS AS (left_value + right_value) STORED); INSERT INTO $database.generated_probe (id,left_value,right_value) VALUES (1,2,3); SELECT id,left_value,right_value,total_value FROM $database.generated_probe; SELECT COLUMN_NAME,EXTRA,GENERATION_EXPRESSION FROM information_schema.COLUMNS WHERE TABLE_SCHEMA='$database' AND TABLE_NAME='generated_probe' ORDER BY ORDINAL_POSITION" },
        [pscustomobject]@{ Name = "generated column index"; Sql = "CREATE TABLE $database.generated_index_probe (id INT PRIMARY KEY, raw_value VARCHAR(32), normalized_value VARCHAR(32) GENERATED ALWAYS AS (LOWER(raw_value)) STORED, INDEX idx_normalized (normalized_value)); INSERT INTO $database.generated_index_probe (id,raw_value) VALUES (1,'AbC'); SELECT id,raw_value,normalized_value FROM $database.generated_index_probe WHERE normalized_value='abc'" },
        [pscustomobject]@{ Name = "common duplicate and unknown errors"; Sql = "CREATE TABLE $database.error_probe (id INT PRIMARY KEY, value VARCHAR(2) NOT NULL); INSERT INTO $database.error_probe VALUES (1,'ok'); INSERT INTO $database.error_probe VALUES (1,'dup'); SELECT missing_column FROM $database.error_probe; INSERT INTO $database.error_probe (id) VALUES (2)" },
        [pscustomobject]@{ Name = "strict data too long error"; Sql = "CREATE TABLE $database.error_length_probe (id INT PRIMARY KEY, value VARCHAR(2) NOT NULL); INSERT INTO $database.error_length_probe VALUES (1,'long')" },
        [pscustomobject]@{ Name = "warning insert ignore conversion"; Sql = "CREATE TABLE $database.warning_probe (id INT PRIMARY KEY, value VARCHAR(2)); INSERT IGNORE INTO $database.warning_probe VALUES (1,'long'),(1,'dup'); SHOW COUNT(*) WARNINGS; SHOW WARNINGS; SELECT id,value FROM $database.warning_probe ORDER BY id" },
        [pscustomobject]@{ Name = "generated column update"; Sql = "CREATE TABLE $database.generated_update_probe (id INT PRIMARY KEY, left_value INT, right_value INT, total_value INT GENERATED ALWAYS AS (left_value + right_value) STORED); INSERT INTO $database.generated_update_probe (id,left_value,right_value) VALUES (1,2,3); UPDATE $database.generated_update_probe SET left_value=7 WHERE id=1; SELECT id,left_value,right_value,total_value FROM $database.generated_update_probe" },
        [pscustomobject]@{ Name = "generated column explicit write errors"; Sql = "CREATE TABLE $database.generated_write_probe (id INT PRIMARY KEY, base_value INT, total_value INT GENERATED ALWAYS AS (base_value + 1) STORED); INSERT INTO $database.generated_write_probe (id,base_value,total_value) VALUES (1,2,99)" },
        [pscustomobject]@{ Name = "generated column update error"; Sql = "UPDATE $database.generated_write_probe SET total_value=99 WHERE id=1" },
        [pscustomobject]@{ Name = "generated column upsert error"; Sql = "CREATE TABLE $database.generated_upsert_probe (id INT PRIMARY KEY, base_value INT, total_value INT GENERATED ALWAYS AS (base_value + 1) STORED); INSERT INTO $database.generated_upsert_probe (id,base_value) VALUES (1,2); INSERT INTO $database.generated_upsert_probe (id,base_value) VALUES (1,3) ON DUPLICATE KEY UPDATE total_value=99" },
        [pscustomobject]@{ Name = "generated column insert-select error"; Sql = "CREATE TABLE $database.generated_select_source (id INT PRIMARY KEY, base_value INT); INSERT INTO $database.generated_select_source VALUES (1,2); CREATE TABLE $database.generated_select_target (id INT PRIMARY KEY, base_value INT, total_value INT GENERATED ALWAYS AS (base_value + 1) STORED); INSERT INTO $database.generated_select_target (id,base_value,total_value) SELECT id,base_value,99 FROM $database.generated_select_source" },
        [pscustomobject]@{ Name = "fulltext metadata"; Sql = "CREATE TABLE $database.fulltext_docs (id INT PRIMARY KEY, title VARCHAR(255), body TEXT, FULLTEXT KEY ft_title_body (title,body)); SELECT INDEX_NAME,INDEX_TYPE FROM information_schema.STATISTICS WHERE TABLE_SCHEMA='$database' AND TABLE_NAME='fulltext_docs' ORDER BY INDEX_NAME,SEQ_IN_INDEX" },
        [pscustomobject]@{ Name = "fulltext boolean search"; Sql = "INSERT INTO $database.fulltext_docs VALUES (1,'Rust database','MySQL compatible database'),(2,'Other','unrelated text'); SELECT id,MATCH(title,body) AGAINST ('+database' IN BOOLEAN MODE) > 0 AS matched FROM $database.fulltext_docs ORDER BY id" },
        [pscustomobject]@{ Name = "fulltext relevance and query operators"; Sql = "SELECT id,ROUND(MATCH(title,body) AGAINST ('database'),6) AS natural_score, MATCH(title,body) AGAINST ('+database -unrelated' IN BOOLEAN MODE) > 0 AS required_score, MATCH(title,body) AGAINST ('`"MySQL compatible`"' IN BOOLEAN MODE) > 0 AS phrase_score, MATCH(title,body) AGAINST ('dat*' IN BOOLEAN MODE) > 0 AS prefix_score FROM $database.fulltext_docs ORDER BY id" },
        [pscustomobject]@{ Name = "fulltext stopwords and query expansion"; Sql = "SELECT id,MATCH(title,body) AGAINST ('the') AS stopword_score,MATCH(title,body) AGAINST ('db') AS short_score,MATCH(title,body) AGAINST ('database' WITH QUERY EXPANSION) > 0 AS expansion_match FROM $database.fulltext_docs ORDER BY id" },
        [pscustomobject]@{ Name = "spatial metadata"; Sql = "CREATE TABLE $database.geo_points (id INT PRIMARY KEY, p POINT NOT NULL, SPATIAL INDEX sp_p (p)); SELECT INDEX_NAME,INDEX_TYPE,SUB_PART FROM information_schema.STATISTICS WHERE TABLE_SCHEMA='$database' AND TABLE_NAME='geo_points' ORDER BY INDEX_NAME,SEQ_IN_INDEX" },
        [pscustomobject]@{ Name = "spatial functions"; Sql = "INSERT INTO $database.geo_points VALUES (1,ST_GeomFromText('POINT(1 2)')); SELECT ST_AsText(p),ST_X(p),ST_Y(p),ST_GeometryType(p) FROM $database.geo_points" },
        [pscustomobject]@{ Name = "alter fulltext and spatial indexes"; Sql = "CREATE TABLE $database.alter_indexes (id INT PRIMARY KEY, body TEXT, p POINT NOT NULL); ALTER TABLE $database.alter_indexes ADD FULLTEXT KEY ft_body (body); ALTER TABLE $database.alter_indexes ADD SPATIAL INDEX sp_p (p); SELECT INDEX_NAME,INDEX_TYPE FROM information_schema.STATISTICS WHERE TABLE_SCHEMA='$database' AND TABLE_NAME='alter_indexes' ORDER BY INDEX_NAME,SEQ_IN_INDEX" },
        [pscustomobject]@{ Name = "nullable spatial index error"; Sql = "CREATE TABLE $database.geo_nullable (id INT PRIMARY KEY, p POINT, SPATIAL INDEX sp_p (p))" },
        [pscustomobject]@{ Name = "non geometry spatial index error"; Sql = "CREATE TABLE $database.geo_invalid (id INT PRIMARY KEY, value VARCHAR(32) NOT NULL, SPATIAL INDEX sp_value (value))" },
        [pscustomobject]@{ Name = "qualified unknown function error"; Sql = "SELECT $database.no_such_function(1)" },
        [pscustomobject]@{ Name = "set operation"; Sql = "SELECT id FROM $database.accounts WHERE id=1 UNION ALL SELECT id FROM $database.accounts WHERE id=2 ORDER BY id" },
        [pscustomobject]@{ Name = "user variables"; Sql = "SET @diff_value=4; SELECT @diff_value+2 AS value; SET @diff_value=NULL; SELECT COALESCE(@diff_value,9) AS fallback" },
        [pscustomobject]@{ Name = "transaction visibility"; Sql = "START TRANSACTION; INSERT INTO $database.accounts VALUES (3,'gamma',3.50); SELECT COUNT(*) AS inside_count FROM $database.accounts; ROLLBACK; SELECT COUNT(*) AS outside_count FROM $database.accounts" },
        [pscustomobject]@{ Name = "savepoint rollback"; Sql = "START TRANSACTION; INSERT INTO $database.accounts VALUES (3,'gamma',3.50); SAVEPOINT before_second; INSERT INTO $database.accounts VALUES (4,'delta',4.50); ROLLBACK TO SAVEPOINT before_second; RELEASE SAVEPOINT before_second; COMMIT; SELECT id,name,amount FROM $database.accounts WHERE id>=3 ORDER BY id" },
        [pscustomobject]@{ Name = "transaction characteristics"; Sql = "SET SESSION TRANSACTION ISOLATION LEVEL READ COMMITTED; SET SESSION TRANSACTION READ ONLY; SELECT @@transaction_isolation AS isolation_level,@@transaction_read_only AS read_only; SET SESSION TRANSACTION ISOLATION LEVEL REPEATABLE READ; SET SESSION TRANSACTION READ WRITE" },
        [pscustomobject]@{ Name = "replace and duplicate update"; Sql = "CREATE TABLE $database.conflict_probe (id INT PRIMARY KEY, value VARCHAR(16), updated INT DEFAULT 0, UNIQUE KEY uq_value(value)); INSERT INTO $database.conflict_probe VALUES (1,'a',0); REPLACE INTO $database.conflict_probe (id,value) VALUES (1,'b'); INSERT INTO $database.conflict_probe (id,value) VALUES (2,'b') ON DUPLICATE KEY UPDATE updated=updated+1,value=CONCAT(value,'-u'); SELECT id,value,updated FROM $database.conflict_probe ORDER BY id" },
        [pscustomobject]@{ Name = "insert select"; Sql = "CREATE TABLE $database.select_source (id INT PRIMARY KEY, value INT); INSERT INTO $database.select_source VALUES (1,10),(2,20); CREATE TABLE $database.select_target (id INT PRIMARY KEY, value INT); INSERT INTO $database.select_target SELECT id,value+1 FROM $database.select_source; SELECT id,value FROM $database.select_target ORDER BY id" },
        [pscustomobject]@{ Name = "update and delete join"; Sql = "USE $database; CREATE TABLE join_left (id INT PRIMARY KEY, value INT); CREATE TABLE join_right (id INT PRIMARY KEY, multiplier INT); INSERT INTO join_left VALUES (1,2),(2,3); INSERT INTO join_right VALUES (1,10),(2,20); UPDATE join_left l JOIN join_right r ON l.id=r.id SET l.value=l.value*r.multiplier WHERE l.id=1; DELETE l FROM join_left l JOIN join_right r ON l.id=r.id WHERE r.id=2; SELECT id,value FROM join_left ORDER BY id" },
        [pscustomobject]@{ Name = "foreign key cascade"; Sql = "CREATE TABLE $database.fk_parent (id INT PRIMARY KEY); CREATE TABLE $database.fk_child (id INT PRIMARY KEY, parent_id INT, CONSTRAINT fk_parent_id FOREIGN KEY (parent_id) REFERENCES $database.fk_parent(id) ON DELETE CASCADE); INSERT INTO $database.fk_parent VALUES (1); INSERT INTO $database.fk_child VALUES (10,1); DELETE FROM $database.fk_parent WHERE id=1; SELECT COUNT(*) AS child_count FROM $database.fk_child" },
        [pscustomobject]@{ Name = "check constraint"; Sql = "CREATE TABLE $database.check_probe (id INT PRIMARY KEY, value INT, CONSTRAINT value_positive CHECK (value>0)); INSERT INTO $database.check_probe VALUES (1,2); SELECT id,value FROM $database.check_probe; INSERT INTO $database.check_probe VALUES (2,-1)" },
        [pscustomobject]@{ Name = "view dml"; Sql = "CREATE TABLE $database.view_base (id INT PRIMARY KEY, value INT); INSERT INTO $database.view_base VALUES (1,2); CREATE VIEW $database.view_probe AS SELECT id,value FROM $database.view_base WHERE value>0 WITH CASCADED CHECK OPTION; UPDATE $database.view_probe SET value=3 WHERE id=1; SELECT id,value FROM $database.view_probe; DELETE FROM $database.view_probe WHERE id=1; SELECT COUNT(*) AS remaining FROM $database.view_base" },
        [pscustomobject]@{ Name = "stored procedure"; Sql = "CREATE PROCEDURE $database.proc_probe(IN p INT) SELECT p+1 AS next_value; CALL $database.proc_probe(4); SELECT ROUTINE_NAME,ROUTINE_TYPE FROM information_schema.ROUTINES WHERE ROUTINE_SCHEMA='$database' AND ROUTINE_NAME='proc_probe'; DROP PROCEDURE $database.proc_probe" },
        [pscustomobject]@{ Name = "trigger mutation"; Sql = "CREATE TABLE $database.trigger_base (id INT PRIMARY KEY, value INT); CREATE TABLE $database.trigger_audit (id INT, old_value INT, new_value INT); CREATE TRIGGER $database.trigger_probe BEFORE UPDATE ON $database.trigger_base FOR EACH ROW INSERT INTO $database.trigger_audit VALUES (OLD.id,OLD.value,NEW.value); INSERT INTO $database.trigger_base VALUES (1,2); UPDATE $database.trigger_base SET value=5 WHERE id=1; SELECT id,old_value,new_value FROM $database.trigger_audit; DROP TRIGGER $database.trigger_probe" },
        [pscustomobject]@{ Name = "role and grants"; Sql = "CREATE USER 'diff_user'@'%' IDENTIFIED BY 'diff-password'; GRANT SELECT ON $database.* TO 'diff_user'@'%'; CREATE ROLE 'diff_role'; GRANT SELECT ON $database.* TO 'diff_role'; GRANT 'diff_role' TO 'diff_user'@'%'; SHOW GRANTS FOR 'diff_user'@'%'; REVOKE 'diff_role' FROM 'diff_user'@'%'; DROP ROLE 'diff_role'; DROP USER 'diff_user'@'%'" },
        [pscustomobject]@{ Name = "collation semantics"; Sql = "CREATE TABLE $database.collation_probe (value VARCHAR(32) COLLATE utf8mb4_0900_ai_ci); INSERT INTO $database.collation_probe VALUES ('resume'),(CONVERT(X'72C3A973756DC3A9' USING utf8mb4)),('Resume'); SELECT HEX(value),OCTET_LENGTH(value),CHAR_LENGTH(value) FROM $database.collation_probe WHERE value='RESUME' ORDER BY value; SELECT COUNT(DISTINCT value) AS distinct_count FROM $database.collation_probe" },
        [pscustomobject]@{ Name = "accounts stability after mixed DDL"; Sql = "SELECT id,name,amount FROM $database.accounts ORDER BY id" },
        [pscustomobject]@{ Name = "explain"; Sql = "EXPLAIN SELECT id,name FROM $database.accounts WHERE id=1; EXPLAIN FORMAT=JSON SELECT id FROM $database.accounts WHERE name='alpha'" },
        [pscustomobject]@{ Name = "show status and table status"; Sql = "SHOW TABLE STATUS FROM $database LIKE 'accounts'; SHOW STATUS LIKE 'Threads_connected'; SHOW VARIABLES LIKE 'max_connections'" },
        [pscustomobject]@{ Name = "columns metadata"; Sql = "SELECT COLUMN_NAME,DATA_TYPE,COLUMN_TYPE,IS_NULLABLE,COLUMN_KEY FROM information_schema.COLUMNS WHERE TABLE_SCHEMA='$database' AND TABLE_NAME='accounts' ORDER BY ORDINAL_POSITION" },
        [pscustomobject]@{ Name = "statistics metadata"; Sql = "SELECT INDEX_NAME,SEQ_IN_INDEX,COLUMN_NAME,NON_UNIQUE FROM information_schema.STATISTICS WHERE TABLE_SCHEMA='$database' AND TABLE_NAME='accounts' ORDER BY INDEX_NAME,SEQ_IN_INDEX" },
        [pscustomobject]@{ Name = "tables metadata"; Sql = "SELECT TABLE_SCHEMA,TABLE_NAME,ENGINE,TABLE_TYPE FROM information_schema.TABLES WHERE TABLE_SCHEMA='$database' AND TABLE_NAME='accounts'" },
        [pscustomobject]@{ Name = "full columns"; Sql = "SHOW FULL COLUMNS FROM $database.accounts" },
        [pscustomobject]@{ Name = "show index"; Sql = "SHOW INDEX FROM $database.accounts" },
        [pscustomobject]@{ Name = "optional metadata views"; Sql = "SELECT TABLE_NAME FROM information_schema.TABLES WHERE TABLE_SCHEMA='information_schema' AND TABLE_NAME IN ('COLUMN_STATISTICS','COLUMNS_EXTENSIONS','INNODB_TABLES','INNODB_TRX','TABLES_EXTENSIONS','USER_ATTRIBUTES') ORDER BY TABLE_NAME" },
        [pscustomobject]@{ Name = "innodb transaction metadata"; Sql = "SELECT COLUMN_NAME FROM information_schema.COLUMNS WHERE TABLE_SCHEMA='information_schema' AND TABLE_NAME='INNODB_TRX' ORDER BY ORDINAL_POSITION" },
        [pscustomobject]@{ Name = "missing table error"; Sql = "SELECT * FROM $database.no_such_table" }
    )

    $failed = 0
    foreach ($case in $cases) {
        if (-not (Invoke-DiffCase -Name $case.Name -Sql $case.Sql)) {
            $failed++
        }
    }
    $sslProbeMysql = Invoke-MySqlClient -Target mysql84 -Sql "SHOW STATUS LIKE 'ssl_version'"
    $sslProbeMyDb = Invoke-MySqlClient -Target mydb -Sql "SHOW STATUS LIKE 'ssl_version'"
    Assert-Success -Target "mysql84" -Label "DataGrip SSL status probe" -Result $sslProbeMysql
    Assert-Success -Target "mydb" -Label "DataGrip SSL status probe" -Result $sslProbeMyDb
    Write-Host "[OK] datagrip ssl status probe" -ForegroundColor Green
    if ($failed -gt 0) {
        throw "$failed differential case(s) failed"
    }
    Write-Host "MySQL 8.4 differential passed: $($cases.Count) cases" -ForegroundColor Green
} finally {
    try {
        $cleanup = Invoke-MySqlClient -Target mydb -Sql "DROP DATABASE IF EXISTS $database"
        if ($cleanup.ExitCode -ne 0) {
            Write-Warning "MyDB cleanup failed: $($cleanup.Output)"
        }
    } catch {
        Write-Warning "MyDB cleanup skipped: $($_.Exception.Message)"
    }
    docker rm -f $container 2>$null | Out-Null
}
