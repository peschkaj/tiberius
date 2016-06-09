use std::borrow::Cow;
use std::convert::From;
use std::cell::RefCell;
use std::fmt::Debug;
use std::rc::Rc;
use protocol::*;
use conn::{Connection};
use types::{ColumnType, ColumnValue, ToColumnType};
use ::{TargetStream, TdsResult, TdsError};

#[derive(Debug)]
#[doc(hidden)]
pub struct StatementInfo {
    pub column_infos: Vec<ColumnData>,
    /// The handle for e.g. prepared statements
    pub handle: Option<u32>,
}

impl StatementInfo {
    pub fn new() -> StatementInfo {
        StatementInfo {
            column_infos: vec![],
            handle: None,
        }
    }
}

/// A result row of a resultset of a query
#[derive(Debug)]
pub struct Row<'a> {
    stmt: Rc<RefCell<StatementInfo>>,
    values: Vec<ColumnValue<'a>>
}

pub trait RowIndex {
    fn get_index(&self, row: &Row) -> Option<usize>;
}

impl RowIndex for usize {
    #[inline]
    fn get_index(&self, _: &Row) -> Option<usize> {
        Some(*self)
    }
}

impl<'a> RowIndex for &'a str {
    fn get_index(&self, row: &Row) -> Option<usize> {
        for (idx, column) in row.stmt.borrow().column_infos.iter().enumerate() {
            match column.col_name {
                Some(ref col_name) if col_name == *self => return Some(idx),
                _ => ()
            }
        }
        None
    }
}

impl<'a> Row<'a> {
    pub fn get<I: RowIndex + Debug, T>(&'a self, idx: I) -> T where Option<T>: From<&'a ColumnValue<'a>> {
        let idx = match idx.get_index(self) {
            Some(x) => x,
            None => panic!("unknown index: {:?}", idx)
        };
        match From::from(&self.values[idx]) {
            Some(x) => x,
            None => panic!("type mismatch for: {}, got instead: {:?}", idx, self.values[idx])
        }
    }
}

/// The resultset of a query (containing the resulting rows)
#[derive(Debug)]
pub struct QueryResult<'a> {
    rows: Option<Vec<Row<'a>>>,
    //stmt: Rc<RefCell<StatementInfo>>
}

impl<'a> QueryResult<'a> {
    /// return the number of contained rows
    pub fn len(&self) -> usize {
        match self.rows {
            None => 0,
            Some(ref rows) => rows.len()
        }
    }

    /// return the row on a specific index, panics if the idx is out of bounds
    pub fn get(&self, idx: usize) -> &Row {
        match self.rows {
            None => (),
            Some(ref rows) => {
                if rows.len() > idx {
                    return &rows[idx]
                }
            }
        }
        panic!("queryresult: get: idx out of bounds");
    }
}

impl<'a> IntoIterator for QueryResult<'a> {
    type Item = Row<'a>;
    type IntoIter = ::std::vec::IntoIter<Row<'a>>;

    fn into_iter(self) -> Self::IntoIter {
        match self.rows {
            Some(x) => x.into_iter(),
            None => vec![].into_iter()
        }
    }
}

#[doc(hidden)]
pub struct StatementInternal<'a, S: 'a + TargetStream> {
    conn: Connection<'a, S>,
    query: Cow<'a, str>,
    stmt: Rc<RefCell<StatementInfo>>,
}

fn handle_execute_packet(packet: &Packet) -> TdsResult<usize> {
    if let Packet::TokenStream(ref tokens) = *packet {
            for token in tokens {
                match *token {
                    TokenStream::Error(ref err) => {
                        return Err(TdsError::ServerError(err.clone()))
                    },
                    TokenStream::Done(ref done_token) => {
                        assert_eq!(done_token.status, TokenStreamDoneStatus::Count as u16);
                        return Ok(done_token.done_row_count as usize)
                    },
                    _ => return Err(TdsError::Other(format!("exec: unexpected TOKEN {:?}", token)))
                }
            }
    }
    Err(TdsError::Other(format!("exec: Unexpected packet {:?}", packet)))
}

fn handle_query_packet(packet: Packet, stmt: Rc<RefCell<StatementInfo>>) -> TdsResult<QueryResult> {
    let mut query_result = QueryResult {
        rows: None,
    };
    if let Packet::TokenStream(tokens) = packet {
            let mut rows = Vec::with_capacity(tokens.len());
            for token in tokens {
                match token {
                    TokenStream::Error(x) => return Err(TdsError::ServerError(x)),
                    TokenStream::Row(row) => rows.push(Row { values: row.data, stmt: stmt.clone() }),
                    _ => ()
                }
            }
            query_result.rows = Some(rows);
            return Ok(query_result)
    }
    Ok(query_result)
}

impl<'a, S: 'a + TargetStream> StatementInternal<'a, S> {
    pub fn new(conn: Connection<'a, S>, query: Cow<'a, str>) -> StatementInternal<'a, S> {
        StatementInternal {
            conn: conn,
            query: query,
            stmt: Rc::new(RefCell::new(StatementInfo::new()))
        }
    }

    pub fn execute_into_query(self) -> TdsResult<QueryResult<'a>> {
        let mut conn = self.conn.borrow_mut();
        try!(conn.internal_exec(&self.query));
        let packet = try!(try!(conn.opts.stream.read_message()).into_stmt_token_stream(&mut *self.stmt.borrow_mut()));
        handle_query_packet(packet, self.stmt)
    }

    pub fn execute(&mut self) -> TdsResult<usize> {
        let mut conn = self.conn.borrow_mut();
        try!(conn.internal_exec(&self.query));
        let packet = try!(conn.read_packet());
        handle_execute_packet(&packet)
    }
}

pub struct PreparedStatement<'a, S: 'a + TargetStream> {
    conn: Connection<'a, S>,
    stmt: Rc<RefCell<StatementInfo>>,
    sql: Cow<'a, str>,
}

impl<'a, S: 'a + TargetStream> PreparedStatement<'a, S> {
    pub fn new(conn: Connection<'a, S>, sql: Cow<'a, str>) -> TdsResult<PreparedStatement<'a, S>> {
        Ok(PreparedStatement{
            conn: conn,
            sql: sql,
            stmt: Rc::new(RefCell::new(StatementInfo::new())),
        })
    }

    /// Prepares the actual statement (sp_prepare)
    fn do_prepare(&self, stmt: &mut StatementInfo, params: &[&ToColumnType]) -> TdsResult<()> {
        let mut param_str = String::new();
        // determine the types from the given params
        for (i, param) in params.iter().enumerate() {
            if i > 0 {
                param_str.push(',')
            }
            param_str.push_str(&format!("@P{} ", i + 1));
            param_str.push_str(param.column_type());
        }
        // for some reason mssql fails when we pass "handle" as int4 (fixed len) insteadof intn (varlen)
        // because it does not know the type (0x38) - probably since int4 was "deprecated" ages ago?
        let params_meta = vec![
            RpcParamData {
                name: Cow::Borrowed("handle"),
                status_flags: rpc::fByRefValue,
                value: ColumnType::I32(0),
            },
            RpcParamData {
                name: Cow::Borrowed("params"),
                status_flags: 0,
                value: ColumnType::String(Cow::Owned(param_str))
            },
            RpcParamData {
                name: Cow::Borrowed("stmt"),
                status_flags: 0,
                value: ColumnType::String(self.sql.clone()),
            }
        ];
        let rpc_req = RpcRequestData {
            proc_id: RpcProcIdValue::Id(RpcProcId::SpPrepare),
            flags: 0,
            params: params_meta,
        };
        let rpc_packet = Packet::RpcRequest(&rpc_req);
        let mut conn = self.conn.borrow_mut();
        try!(conn.send_packet(&rpc_packet));
        {
            let packet = try!(try!(conn.opts.stream.read_message()).into_stmt_token_stream(stmt));
            try!(packet.catch_error());
            match packet {
                Packet::TokenStream(ref tokens) => {
                    for token in tokens {
                        match *token {
                            TokenStream::ReturnValue(ref retval) if retval.name == "handle" => {
                                if let Some(ColumnValue::Some(ColumnType::I32(ihandle))) = retval.data {
                                    stmt.handle = Some(ihandle as u32);
                                } else {
                                    return Err(TdsError::Other(format!("prepare: invalid handle id {:?}", tokens)))
                                }
                            },
                            _ => ()
                        }
                    }
                },
                _ => return Err(TdsError::Other(format!("exec: Unexpected packet {:?}", packet)))
            }
            if stmt.handle.is_none() {
                return Err(TdsError::Other(format!("prepare: did not receive a handle id {:?}", packet)))
            }
        }
        Ok(())
    }

    /// Execute the statement (sp_execute)
    #[inline]
    fn do_internal_exec(&self, stmt: &mut StatementInfo, params: &[&ToColumnType]) -> TdsResult<()> {
        let mut params_meta = vec![
            RpcParamData {
                name: Cow::Borrowed("handle"),
                status_flags: rpc::fByRefValue,
                value: ColumnType::I32(stmt.handle.unwrap() as i32),
            },
        ];
        for (i, param) in params.iter().enumerate() {
            params_meta.push(RpcParamData {
                name: Cow::Owned(format!("@P{}", i+1)),
                status_flags: 0,
                value: param.to_column_type(),
            });
        }

        let rpc_req = RpcRequestData {
            // as freeTDS, use sp_execute since SpPrepare (as int) seems broken, even microsofts odbc driver seems to use this
            proc_id: RpcProcIdValue::Name(Cow::Borrowed("sp_execute")),
            flags: rpc::fNoMetaData,
            params: params_meta,
        };
        let rpc_packet = Packet::RpcRequest(&rpc_req);
        let mut conn = self.conn.borrow_mut();
        try!(conn.send_packet(&rpc_packet));
        Ok(())
    }

    /// Makes sure the statement is prepared, since we lazily prepare statements
    /// and then executes the statement, handling it as a query and therefore returning the results as rows
    pub fn query<'b>(&self, params: &[&ToColumnType]) -> TdsResult<QueryResult<'b>> {
        let packet;
        {
            let mut stmt = &mut * self.stmt.borrow_mut();
            if stmt.handle.is_none() {
                try!(self.do_prepare(stmt, params));
            }
            try!(self.do_internal_exec(stmt, params));
            let mut conn = self.conn.borrow_mut();
            packet = try!(try!(conn.opts.stream.read_message()).into_stmt_token_stream(stmt));
        }
        handle_query_packet(packet, self.stmt.clone())
    }
}

pub struct ParameterizedStatement<'a, S: 'a + TargetStream> {
    conn: Connection<'a, S>,
    sql: Cow<'a, str>,
    stmt: Rc<RefCell<StatementInfo>>,
    parameter_meta: Option<Cow<'a, str>>,
    parameters: Option<&'a [&'a ToColumnType]>,
}

impl<'a, S: 'a + TargetStream> ParameterizedStatement<'a, S> {
    pub fn new(conn: Connection<'a, S>,
               sql: Cow<'a, str>,
               parameter_meta: Option<Cow<'a, str>>)
               -> TdsResult<ParameterizedStatement<'a, S>>
    {
        Ok(ParameterizedStatement {
            conn: conn,
            sql: sql,
            parameter_meta: parameter_meta,
            stmt: Rc::new(RefCell::new(StatementInfo::new())),
            parameters: None,
        })
    }

    fn do_internal_exec(&self, param_meta: &str, params: &[&ToColumnType]) -> TdsResult<()> {
        let mut params_meta = vec![
            RpcParamData {
                name: Cow::Borrowed("stmt"),
                status_flags: 0,
                value: ColumnType::String(self.sql.clone()),
            },
            RpcParamData {
                name: Cow::Borrowed("params"),
                status_flags: 0,
                value: ColumnType::String(Cow::Borrowed(param_meta.clone())),
            }
        ];

        for (i, param) in params.iter().enumerate() {
            params_meta.push(RpcParamData {
                name: Cow::Owned(format!("@P{}", i+1)),
                status_flags: 0,
                value: param.to_column_type(),
            });
        }

        let rpc_req = RpcRequestData {
            proc_id: RpcProcIdValue::Id(RpcProcId::SpExecuteSql),
            flags: 0,
            params: params_meta,
        };

        let rpc_packet = Packet::RpcRequest(&rpc_req);
        let mut conn = self.conn.borrow_mut();

        try!(conn.send_packet(&rpc_packet));
        Ok(())
    }

    // TODO: `params` needs to be name value pairs in the form of "@param": "whatever"
    /* TODO: need to add a new method to `Connection` in conn.rs at line 158ish.
             This method should create a parameterized statement and accept sql and parameter metadata.
     */
    pub fn query<'b>(&self, param_meta: &str, params: &[&ToColumnType]) -> TdsResult<QueryResult<'b>> {
        let packet;
        {
            let mut conn = self.conn.borrow_mut();
            packet = try!(try!(conn.opts.stream.read_message()).into_stmt_token_stream(&mut self.stmt.borrow_mut()));

            unimplemented!();
        }
    }
}
