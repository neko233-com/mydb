//! Small, dependency-free SQL statement parser used by the parser CLI.
//!
//! The server has its own MySQL execution parser in `mydb-wire`. This crate is
//! intentionally a stable, lightweight inspection API: it parses the common
//! statement shapes without pretending to execute or validate the full MySQL
//! grammar.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parser;

impl Parser {
    pub fn new() -> Self {
        Self
    }

    pub fn parse(&self, sql: &str) -> Result<Statement, ParseError> {
        let source = trim_statement(sql);
        if source.is_empty() {
            return Err(ParseError::new("statement is empty", 0));
        }

        let (keyword, remainder) = first_word(source);
        match keyword.to_ascii_uppercase().as_str() {
            "SELECT" => parse_select(source),
            "INSERT" => parse_insert(source),
            "UPDATE" => parse_update(source),
            "DELETE" => parse_delete(source),
            "CREATE" => parse_create(source),
            "DROP" => parse_drop(source),
            "ALTER" => parse_alter(source),
            "USE" => parse_use(remainder),
            "SHOW" => parse_show(remainder),
            _ => Ok(Statement::Unknown(source.to_string())),
        }
    }
}

impl Default for Parser {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Statement {
    Select(SelectStatement),
    Insert(InsertStatement),
    Update(UpdateStatement),
    Delete(DeleteStatement),
    Create(CreateStatement),
    Drop(DropStatement),
    Alter(AlterStatement),
    Use(String),
    Show(ShowStatement),
    Unknown(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectStatement {
    pub columns: Vec<String>,
    pub table: String,
    pub where_clause: Option<String>,
    pub order_by: Option<String>,
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InsertStatement {
    pub table: String,
    pub columns: Vec<String>,
    pub values: Vec<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateStatement {
    pub table: String,
    pub set_clause: Vec<(String, String)>,
    pub where_clause: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeleteStatement {
    pub table: String,
    pub where_clause: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateStatement {
    pub table_name: String,
    pub columns: Vec<ColumnDef>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnDef {
    pub name: String,
    pub data_type: String,
    pub nullable: bool,
    pub primary_key: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DropStatement {
    pub table_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlterStatement {
    pub table_name: String,
    pub operation: AlterOperation,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AlterOperation {
    AddColumn(ColumnDef),
    DropColumn(String),
    ModifyColumn(ColumnDef),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShowStatement {
    pub target: ShowTarget,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShowTarget {
    Databases,
    Tables,
    Columns(String),
    CreateTable(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub message: String,
    pub position: usize,
}

impl ParseError {
    fn new(message: impl Into<String>, position: usize) -> Self {
        Self {
            message: message.into(),
            position,
        }
    }
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Parse error at position {}: {}",
            self.position, self.message
        )
    }
}

impl std::error::Error for ParseError {}

fn trim_statement(sql: &str) -> &str {
    let source = sql.trim();
    source.strip_suffix(';').map(str::trim).unwrap_or(source)
}

fn first_word(value: &str) -> (&str, &str) {
    let split = value
        .char_indices()
        .find(|(_, character)| character.is_whitespace())
        .map(|(index, _)| index)
        .unwrap_or(value.len());
    (&value[..split], value[split..].trim_start())
}

fn parse_select(source: &str) -> Result<Statement, ParseError> {
    let body = source["SELECT".len()..].trim_start();
    let (projection, from) = split_keyword(body, "FROM");
    let (table, where_clause, order_by, limit) = if let Some(from) = from {
        let (table, tail) = take_first_word(from);
        let (where_clause, order_by, limit) = parse_select_tail(tail)?;
        (unquote_identifier(table), where_clause, order_by, limit)
    } else {
        (String::new(), None, None, None)
    };
    Ok(Statement::Select(SelectStatement {
        columns: split_top_level(projection, ',')?,
        table,
        where_clause,
        order_by,
        limit,
    }))
}

type SelectTail = (Option<String>, Option<String>, Option<u32>);

fn parse_select_tail(tail: &str) -> Result<SelectTail, ParseError> {
    let (without_limit, limit_part) = split_keyword(tail, "LIMIT");
    let limit = limit_part.map(parse_limit_value).transpose()?.flatten();
    let (without_order, order_part) = split_keyword(without_limit, "ORDER BY");
    let order_by = order_part.map(|value| value.trim().to_string());
    let (_, where_part) = split_keyword(without_order, "WHERE");
    let where_clause = where_part
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    Ok((where_clause, order_by, limit))
}

fn parse_limit_value(value: &str) -> Result<Option<u32>, ParseError> {
    let number = value
        .split_whitespace()
        .next()
        .ok_or_else(|| ParseError::new("LIMIT requires a number", 0))?;
    number
        .parse()
        .map(Some)
        .map_err(|_| ParseError::new("LIMIT requires a non-negative integer", 0))
}

fn parse_insert(source: &str) -> Result<Statement, ParseError> {
    let body = source["INSERT".len()..].trim_start();
    let body = strip_prefix_ci(body, "INTO").unwrap_or(body).trim_start();
    let (before_values, values_source) = split_keyword(body, "VALUES");
    let Some(values_source) = values_source else {
        return Err(ParseError::new("INSERT requires VALUES", source.len()));
    };
    let values_source = values_source.trim();
    let (table, columns) = if let Some(open) = before_values.find('(') {
        let close = matching_parenthesis(before_values, open)?;
        let table = before_values[..open].trim();
        let columns = split_top_level(&before_values[open + 1..close], ',')?;
        (unquote_identifier(table), columns)
    } else {
        (unquote_identifier(before_values.trim()), Vec::new())
    };
    let mut values = Vec::new();
    for item in split_top_level(values_source, ',')? {
        let item = item.trim();
        if !item.starts_with('(') {
            return Err(ParseError::new("VALUES entries must be parenthesized", 0));
        }
        let close = matching_parenthesis(item, 0)?;
        if !item[close + 1..].trim().is_empty() {
            return Err(ParseError::new(
                "unexpected text after VALUES entry",
                close + 1,
            ));
        }
        values.push(split_top_level(&item[1..close], ',')?);
    }
    Ok(Statement::Insert(InsertStatement {
        table,
        columns,
        values,
    }))
}

fn parse_update(source: &str) -> Result<Statement, ParseError> {
    let body = source["UPDATE".len()..].trim_start();
    let (table, assignments) = split_keyword(body, "SET");
    let Some(assignments) = assignments else {
        return Err(ParseError::new("UPDATE requires SET", source.len()));
    };
    let (assignments, where_clause) = split_keyword(assignments, "WHERE");
    let mut set_clause = Vec::new();
    for assignment in split_top_level(assignments, ',')? {
        let (column, value) = split_equals(&assignment)?;
        set_clause.push((unquote_identifier(column.trim()), value.trim().to_string()));
    }
    Ok(Statement::Update(UpdateStatement {
        table: unquote_identifier(table.trim()),
        set_clause,
        where_clause: where_clause.map(|value| value.trim().to_string()),
    }))
}

fn parse_delete(source: &str) -> Result<Statement, ParseError> {
    let body = source["DELETE".len()..].trim_start();
    let body = strip_prefix_ci(body, "FROM")
        .ok_or_else(|| ParseError::new("DELETE requires FROM", "DELETE".len()))?;
    let (table, where_clause) = split_keyword(body.trim_start(), "WHERE");
    Ok(Statement::Delete(DeleteStatement {
        table: unquote_identifier(table.trim()),
        where_clause: where_clause.map(|value| value.trim().to_string()),
    }))
}

fn parse_create(source: &str) -> Result<Statement, ParseError> {
    let body = source["CREATE".len()..].trim_start();
    let Some(body) = strip_prefix_ci(body, "TABLE") else {
        return Ok(Statement::Unknown(source.to_string()));
    };
    let body = body.trim_start();
    let body = strip_prefix_ci(body, "IF NOT EXISTS")
        .unwrap_or(body)
        .trim_start();
    let open = body
        .find('(')
        .ok_or_else(|| ParseError::new("CREATE TABLE requires column definitions", source.len()))?;
    let close = matching_parenthesis(body, open)?;
    let table_name = unquote_identifier(body[..open].trim());
    let mut columns = Vec::new();
    for definition in split_top_level(&body[open + 1..close], ',')? {
        let definition = definition.trim();
        if is_table_constraint(definition) {
            continue;
        }
        let (name, rest) = take_first_word(definition);
        if rest.is_empty() {
            return Err(ParseError::new("column definition requires a type", 0));
        }
        let upper = rest.to_ascii_uppercase();
        columns.push(ColumnDef {
            name: unquote_identifier(name),
            data_type: rest
                .split_whitespace()
                .take_while(|word| {
                    !matches!(
                        word.to_ascii_uppercase().as_str(),
                        "NOT"
                            | "NULL"
                            | "DEFAULT"
                            | "PRIMARY"
                            | "UNIQUE"
                            | "CHECK"
                            | "REFERENCES"
                            | "COMMENT"
                            | "COLLATE"
                            | "CHARACTER"
                    )
                })
                .collect::<Vec<_>>()
                .join(" "),
            nullable: !upper.contains("NOT NULL"),
            primary_key: upper.contains("PRIMARY KEY"),
        });
    }
    Ok(Statement::Create(CreateStatement {
        table_name,
        columns,
    }))
}

fn parse_drop(source: &str) -> Result<Statement, ParseError> {
    let body = source["DROP".len()..].trim_start();
    let Some(body) = strip_prefix_ci(body, "TABLE") else {
        return Ok(Statement::Unknown(source.to_string()));
    };
    let body = strip_prefix_ci(body.trim_start(), "IF EXISTS").unwrap_or(body.trim_start());
    Ok(Statement::Drop(DropStatement {
        table_name: unquote_identifier(body.trim()),
    }))
}

fn parse_alter(source: &str) -> Result<Statement, ParseError> {
    let body = source["ALTER".len()..].trim_start();
    let Some(body) = strip_prefix_ci(body, "TABLE") else {
        return Ok(Statement::Unknown(source.to_string()));
    };
    let (table, remainder) = take_first_word(body.trim_start());
    let (operation, remainder) = first_word(remainder);
    let operation = match operation.to_ascii_uppercase().as_str() {
        "ADD" => {
            let remainder = strip_prefix_ci(remainder.trim_start(), "COLUMN")
                .unwrap_or(remainder)
                .trim_start();
            AlterOperation::AddColumn(parse_column_definition(remainder)?)
        }
        "MODIFY" => AlterOperation::ModifyColumn(parse_column_definition(remainder)?),
        "DROP" => {
            let remainder = strip_prefix_ci(remainder.trim_start(), "COLUMN")
                .unwrap_or(remainder)
                .trim();
            AlterOperation::DropColumn(unquote_identifier(remainder))
        }
        "CHANGE" => AlterOperation::ModifyColumn(parse_column_definition(remainder)?),
        _ => return Ok(Statement::Unknown(source.to_string())),
    };
    Ok(Statement::Alter(AlterStatement {
        table_name: unquote_identifier(table),
        operation,
    }))
}

fn parse_column_definition(value: &str) -> Result<ColumnDef, ParseError> {
    let (name, rest) = take_first_word(value.trim());
    if rest.is_empty() {
        return Err(ParseError::new("column definition requires a type", 0));
    }
    let upper = rest.to_ascii_uppercase();
    Ok(ColumnDef {
        name: unquote_identifier(name),
        data_type: rest
            .split_whitespace()
            .take_while(|word| {
                !matches!(
                    word.to_ascii_uppercase().as_str(),
                    "NOT" | "NULL" | "DEFAULT" | "PRIMARY" | "UNIQUE"
                )
            })
            .collect::<Vec<_>>()
            .join(" "),
        nullable: !upper.contains("NOT NULL"),
        primary_key: upper.contains("PRIMARY KEY"),
    })
}

fn parse_use(value: &str) -> Result<Statement, ParseError> {
    let (database, remainder) = take_first_word(value);
    if database.is_empty() || !remainder.is_empty() {
        return Err(ParseError::new("USE requires one database name", 0));
    }
    Ok(Statement::Use(unquote_identifier(database)))
}

fn parse_show(value: &str) -> Result<Statement, ParseError> {
    let upper = value.to_ascii_uppercase();
    let target = if upper == "DATABASES" || upper == "SCHEMAS" {
        ShowTarget::Databases
    } else if upper == "TABLES" {
        ShowTarget::Tables
    } else if let Some(name) = strip_prefix_ci(value, "COLUMNS FROM") {
        ShowTarget::Columns(unquote_identifier(name.trim()))
    } else if let Some(name) = strip_prefix_ci(value, "CREATE TABLE") {
        ShowTarget::CreateTable(unquote_identifier(name.trim()))
    } else {
        return Ok(Statement::Unknown(format!("SHOW {value}")));
    };
    Ok(Statement::Show(ShowStatement { target }))
}

fn split_equals(value: &str) -> Result<(&str, &str), ParseError> {
    let mut state = ScanState::default();
    for (index, character) in value.char_indices() {
        state.step(character)?;
        if character == '=' && state.is_top_level() {
            return Ok((&value[..index], &value[index + 1..]));
        }
    }
    Err(ParseError::new("assignment requires '='", 0))
}

fn split_keyword<'a>(value: &'a str, keyword: &str) -> (&'a str, Option<&'a str>) {
    let Some(index) = find_keyword(value, keyword) else {
        return (value, None);
    };
    (&value[..index], Some(&value[index + keyword.len()..]))
}

fn find_keyword(value: &str, keyword: &str) -> Option<usize> {
    let target = keyword.as_bytes();
    let bytes = value.as_bytes();
    let mut state = ScanState::default();
    let mut index = 0;
    while index + target.len() <= bytes.len() {
        let character = value[index..].chars().next()?;
        state.step(character).ok()?;
        if state.is_top_level()
            && value[index..].len() >= target.len()
            && value[index..index + target.len()].eq_ignore_ascii_case(keyword)
            && boundary(value, index, keyword.len())
        {
            return Some(index);
        }
        index += character.len_utf8();
    }
    None
}

fn boundary(value: &str, start: usize, length: usize) -> bool {
    let before = value[..start].chars().next_back();
    let after = value[start + length..].chars().next();
    !before.is_some_and(is_identifier_character) && !after.is_some_and(is_identifier_character)
}

fn is_identifier_character(character: char) -> bool {
    character.is_ascii_alphanumeric() || character == '_' || character == '$'
}

fn split_top_level(value: &str, delimiter: char) -> Result<Vec<String>, ParseError> {
    let mut result = Vec::new();
    let mut state = ScanState::default();
    let mut start = 0;
    for (index, character) in value.char_indices() {
        state.step(character)?;
        if character == delimiter && state.is_top_level() {
            result.push(value[start..index].trim().to_string());
            start = index + character.len_utf8();
        }
    }
    state.finish(value.len())?;
    let tail = value[start..].trim();
    if !tail.is_empty() || !result.is_empty() {
        result.push(tail.to_string());
    }
    Ok(result)
}

fn matching_parenthesis(value: &str, open: usize) -> Result<usize, ParseError> {
    let mut state = ScanState::default();
    for (index, character) in value[open..].char_indices() {
        let absolute = open + index;
        state.step(character)?;
        if character == ')' && state.depth == 0 {
            return Ok(absolute);
        }
    }
    Err(ParseError::new("unclosed parenthesis", open))
}

fn take_first_word(value: &str) -> (&str, &str) {
    let value = value.trim_start();
    let end = value
        .char_indices()
        .find(|(_, character)| character.is_whitespace())
        .map(|(index, _)| index)
        .unwrap_or(value.len());
    (&value[..end], value[end..].trim_start())
}

fn strip_prefix_ci<'a>(value: &'a str, prefix: &str) -> Option<&'a str> {
    let remainder = value.get(prefix.len()..)?;
    value
        .get(..prefix.len())
        .filter(|head| head.eq_ignore_ascii_case(prefix))
        .filter(|_| match remainder.chars().next() {
            None => true,
            Some(character) => character.is_whitespace(),
        })
        .map(|_| remainder)
}

fn unquote_identifier(value: &str) -> String {
    let value = value.trim();
    if value.len() >= 2 && value.starts_with('`') && value.ends_with('`') {
        value[1..value.len() - 1].replace("``", "`")
    } else {
        value.to_string()
    }
}

fn is_table_constraint(value: &str) -> bool {
    let (keyword, _) = first_word(value);
    matches!(
        keyword.to_ascii_uppercase().as_str(),
        "PRIMARY" | "UNIQUE" | "KEY" | "INDEX" | "CONSTRAINT" | "FOREIGN" | "CHECK"
    )
}

#[derive(Default)]
struct ScanState {
    quote: Option<char>,
    escaped: bool,
    depth: usize,
}

impl ScanState {
    fn step(&mut self, character: char) -> Result<(), ParseError> {
        if self.escaped {
            self.escaped = false;
            return Ok(());
        }
        if self.quote.is_some() && character == '\\' {
            self.escaped = true;
            return Ok(());
        }
        if let Some(quote) = self.quote {
            if character == quote {
                self.quote = None;
            }
            return Ok(());
        }
        match character {
            '\'' | '"' | '`' => self.quote = Some(character),
            '(' => self.depth = self.depth.saturating_add(1),
            ')' if self.depth > 0 => self.depth -= 1,
            _ => {}
        }
        Ok(())
    }

    fn finish(&self, position: usize) -> Result<(), ParseError> {
        if self.quote.is_some() {
            return Err(ParseError::new("unterminated quoted value", position));
        }
        if self.depth != 0 {
            return Err(ParseError::new("unclosed parenthesis", position));
        }
        Ok(())
    }

    fn is_top_level(&self) -> bool {
        self.quote.is_none() && self.depth == 0
    }
}

impl std::fmt::Display for Statement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

#[cfg(test)]
mod tests {
    use super::{AlterOperation, Parser, ShowTarget, Statement};

    #[test]
    fn parses_select_clauses_without_splitting_quoted_keywords() {
        let statement = Parser::new()
            .parse(
                "SELECT id, 'FROM value' FROM `users` WHERE name='ORDER BY' ORDER BY id LIMIT 5;",
            )
            .expect("valid select");
        let Statement::Select(statement) = statement else {
            panic!("expected select statement");
        };
        assert_eq!(statement.table, "users");
        assert_eq!(statement.columns, vec!["id", "'FROM value'"]);
        assert_eq!(statement.where_clause.as_deref(), Some("name='ORDER BY'"));
        assert_eq!(statement.order_by.as_deref(), Some("id"));
        assert_eq!(statement.limit, Some(5));
    }

    #[test]
    fn parses_insert_update_delete_and_create() {
        let parser = Parser::new();
        let Statement::Insert(insert) = parser
            .parse("INSERT INTO t (id, name) VALUES (1, 'a,b'), (2, 'c')")
            .expect("valid insert")
        else {
            panic!("expected insert statement");
        };
        assert_eq!(insert.values[0], vec!["1", "'a,b'"]);

        let Statement::Update(update) = parser
            .parse("UPDATE t SET name='x', score=score+1 WHERE id=1")
            .expect("valid update")
        else {
            panic!("expected update statement");
        };
        assert_eq!(update.set_clause[1], ("score".into(), "score+1".into()));

        let Statement::Delete(delete) = parser
            .parse("DELETE FROM t WHERE id=1")
            .expect("valid delete")
        else {
            panic!("expected delete statement");
        };
        assert_eq!(delete.table, "t");

        let Statement::Create(create) = parser
            .parse("CREATE TABLE t (id BIGINT PRIMARY KEY, name VARCHAR(32) NOT NULL)")
            .expect("valid create")
        else {
            panic!("expected create statement");
        };
        assert_eq!(create.columns[0].data_type, "BIGINT");
        assert!(create.columns[0].primary_key);
        assert_eq!(create.columns[1].data_type, "VARCHAR(32)");
        assert!(!create.columns[1].nullable);
    }

    #[test]
    fn parses_ddl_and_show_targets() {
        let parser = Parser::new();
        let Statement::Alter(alter) = parser
            .parse("ALTER TABLE t ADD COLUMN active BOOLEAN")
            .expect("valid alter")
        else {
            panic!("expected alter statement");
        };
        assert!(matches!(alter.operation, AlterOperation::AddColumn(_)));

        let Statement::Show(show) = parser.parse("SHOW COLUMNS FROM t").expect("valid show") else {
            panic!("expected show statement");
        };
        assert_eq!(show.target, ShowTarget::Columns("t".into()));
    }

    #[test]
    fn rejects_unbalanced_sql_and_preserves_unknown_statements() {
        assert!(Parser::new().parse("SELECT ('broken'").is_err());
        assert!(matches!(
            Parser::new().parse("FLUSH TABLES").expect("unknown is valid"),
            Statement::Unknown(value) if value == "FLUSH TABLES"
        ));
    }
}
