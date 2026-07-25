//! WASM prepared statement that executes against server-side SQLite.
//!
//! Bind parameters are collected locally; `step` / `exec` / `rows` send one
//! `Sql::query` round-trip via the sync HTTP bridge.

use std::cell::RefCell;

use anyhow::{Result, bail};

use crate::bindable::{Bind, Column};
use crate::connection::Connection;
use crate::remote_sql::{self, SqlCell, SqlParam, SqlQueryResult};

pub struct Statement<'a> {
    #[allow(dead_code)]
    connection: &'a Connection,
    sql: String,
    /// 1-based SQLite bind slots. RefCell: `Bind` takes `&Statement`.
    binds: RefCell<Vec<Option<SqlParam>>>,
    executed: bool,
    result: SqlQueryResult,
    /// Index of the next row to yield from `step` (0-based into result.rows).
    next_row: usize,
    /// Currently active row for column_* readers.
    current_row: Option<Vec<SqlCell>>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StepResult {
    Row,
    Done,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SqlType {
    Text,
    Integer,
    Blob,
    Float,
    Null,
}

impl<'a> Statement<'a> {
    pub fn prepare<T: AsRef<str>>(connection: &'a Connection, query: T) -> Result<Self> {
        Ok(Self {
            connection,
            sql: query.as_ref().to_string(),
            binds: RefCell::new(Vec::new()),
            executed: false,
            result: SqlQueryResult::default(),
            next_row: 0,
            current_row: None,
        })
    }

    pub fn reset(&mut self) {
        self.executed = false;
        self.result = SqlQueryResult::default();
        self.next_row = 0;
        self.current_row = None;
    }

    pub fn parameter_count(&self) -> i32 {
        self.binds.borrow().len() as i32
    }

    fn set_bind_shared(&self, index: i32, value: SqlParam) {
        let mut binds = self.binds.borrow_mut();
        let idx = index as usize;
        if idx == 0 {
            return;
        }
        if binds.len() < idx {
            binds.resize(idx, None);
        }
        binds[idx - 1] = Some(value);
    }

    pub fn bind_blob(&self, index: i32, blob: &[u8]) -> Result<()> {
        self.set_bind_shared(index, SqlParam::blob(blob));
        Ok(())
    }

    pub fn column_blob(&mut self, index: i32) -> Result<&[u8]> {
        match self
            .current_row
            .as_ref()
            .and_then(|r| r.get(index as usize))
        {
            Some(SqlCell::Blob(b)) => {
                let ptr = b.as_ptr();
                let len = b.len();
                // SAFETY: `current_row` owns the bytes for the lifetime of this borrow.
                unsafe { Ok(std::slice::from_raw_parts(ptr, len)) }
            }
            _ => Ok(&[]),
        }
    }

    pub fn bind_double(&self, index: i32, double: f64) -> Result<()> {
        self.set_bind_shared(index, SqlParam::float(double));
        Ok(())
    }

    pub fn column_double(&self, index: i32) -> Result<f64> {
        Ok(match self.cell_ref(index) {
            Some(SqlCell::Float(f)) => *f,
            Some(SqlCell::Int(i)) => *i as f64,
            Some(SqlCell::Text(s)) => s.parse().unwrap_or(0.0),
            _ => 0.0,
        })
    }

    pub fn bind_int(&self, index: i32, int: i32) -> Result<()> {
        self.set_bind_shared(index, SqlParam::int(int as i64));
        Ok(())
    }

    pub fn column_int(&self, index: i32) -> Result<i32> {
        Ok(self.column_int64(index)? as i32)
    }

    pub fn bind_int64(&self, index: i32, int: i64) -> Result<()> {
        self.set_bind_shared(index, SqlParam::int(int));
        Ok(())
    }

    pub fn column_int64(&self, index: i32) -> Result<i64> {
        Ok(match self.cell_ref(index) {
            Some(SqlCell::Int(i)) => *i,
            Some(SqlCell::Float(f)) => *f as i64,
            Some(SqlCell::Text(s)) => s.parse().unwrap_or(0),
            _ => 0,
        })
    }

    pub fn bind_null(&self, index: i32) -> Result<()> {
        self.set_bind_shared(index, SqlParam::null());
        Ok(())
    }

    pub fn bind_text(&self, index: i32, text: &str) -> Result<()> {
        self.set_bind_shared(index, SqlParam::text(text));
        Ok(())
    }

    pub fn column_text(&mut self, index: i32) -> Result<&str> {
        // Coerce non-text cells to text storage so we can return a stable &str.
        let idx = index as usize;
        if let Some(row) = self.current_row.as_mut() {
            if let Some(cell) = row.get_mut(idx) {
                match cell {
                    SqlCell::Text(_) => {}
                    SqlCell::Int(i) => *cell = SqlCell::Text(i.to_string()),
                    SqlCell::Float(f) => *cell = SqlCell::Text(f.to_string()),
                    SqlCell::Blob(b) => {
                        *cell = SqlCell::Text(String::from_utf8_lossy(b).into_owned())
                    }
                    SqlCell::Null => *cell = SqlCell::Text(String::new()),
                }
            }
        }
        match self.current_row.as_ref().and_then(|r| r.get(idx)) {
            Some(SqlCell::Text(s)) => Ok(s.as_str()),
            _ => Ok(""),
        }
    }

    pub fn bind<T: Bind>(&self, value: &T, index: i32) -> Result<i32> {
        debug_assert!(index > 0);
        value.bind(self, index)
    }

    pub fn column<T: Column>(&mut self) -> Result<T> {
        Ok(T::column(self, 0)?.0)
    }

    pub fn column_type(&mut self, index: i32) -> Result<SqlType> {
        Ok(match self.cell_ref(index) {
            Some(SqlCell::Null) | None => SqlType::Null,
            Some(SqlCell::Int(_)) => SqlType::Integer,
            Some(SqlCell::Float(_)) => SqlType::Float,
            Some(SqlCell::Text(_)) => SqlType::Text,
            Some(SqlCell::Blob(_)) => SqlType::Blob,
        })
    }

    pub fn with_bindings(&mut self, bindings: &impl Bind) -> Result<&mut Self> {
        self.binds.borrow_mut().clear();
        self.executed = false;
        self.bind(bindings, 1)?;
        Ok(self)
    }

    fn params_vec(&self) -> Vec<SqlParam> {
        self.binds
            .borrow()
            .iter()
            .map(|slot| slot.clone().unwrap_or(SqlParam::Null))
            .collect()
    }

    fn ensure_executed(&mut self) -> Result<()> {
        if self.executed {
            return Ok(());
        }
        let params = self.params_vec();
        self.result = remote_sql::query(&self.sql, &params)?;
        self.executed = true;
        self.next_row = 0;
        self.current_row = None;
        Ok(())
    }

    fn cell_ref(&self, index: i32) -> Option<&SqlCell> {
        self.current_row.as_ref()?.get(index as usize)
    }

    fn step(&mut self) -> Result<StepResult> {
        self.ensure_executed()?;
        if self.next_row < self.result.rows.len() {
            self.current_row = Some(self.result.rows[self.next_row].clone());
            self.next_row += 1;
            Ok(StepResult::Row)
        } else {
            self.current_row = None;
            Ok(StepResult::Done)
        }
    }

    pub fn exec(&mut self) -> Result<()> {
        self.ensure_executed()?;
        self.next_row = self.result.rows.len();
        self.current_row = None;
        Ok(())
    }

    pub fn map<R>(
        &mut self,
        mut callback: impl FnMut(&mut Statement) -> Result<R>,
    ) -> Result<Vec<R>> {
        let mut out = Vec::new();
        loop {
            match self.step()? {
                StepResult::Done => break,
                StepResult::Row => out.push(callback(self)?),
            }
        }
        Ok(out)
    }

    pub fn rows<R: Column>(&mut self) -> Result<Vec<R>> {
        self.map(|this| this.column::<R>())
    }

    pub fn single<R>(&mut self, callback: impl FnOnce(&mut Statement) -> Result<R>) -> Result<R> {
        match self.step()? {
            StepResult::Row => {
                let result = callback(self)?;
                if self.step()? != StepResult::Done {
                    bail!("single called with a query that returns more than one row.");
                }
                Ok(result)
            }
            StepResult::Done => bail!("single called with query that returns no rows."),
        }
    }

    pub fn row<R: Column>(&mut self) -> Result<R> {
        self.single(|this| this.column::<R>())
    }

    pub fn maybe<R>(
        &mut self,
        callback: impl FnOnce(&mut Statement) -> Result<R>,
    ) -> Result<Option<R>> {
        match self.step()? {
            StepResult::Done => Ok(None),
            StepResult::Row => {
                let value = callback(self)?;
                if self.step()? != StepResult::Done {
                    bail!("maybe called with a query that returns more than one row.");
                }
                Ok(Some(value))
            }
        }
    }

    pub fn maybe_row<R: Column>(&mut self) -> Result<Option<R>> {
        self.maybe(|this| this.column::<R>())
    }
}

impl Drop for Statement<'_> {
    fn drop(&mut self) {}
}
