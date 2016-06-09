use std::borrow::Cow;
use std::cell::RefCell;
use std::fmt;
use std::rc::Rc;
use std::io::prelude::*;
use std::net::{TcpStream, ToSocketAddrs};
use std::ops::Deref;

use protocol::*;
use stmt::{StatementInternal, QueryResult, PreparedStatement, ParameterizedStatement};
use ::{TdsResult, TdsError};

#[derive(Debug, PartialEq)]
pub enum ClientState {
    Initial,
    PreloginPerformed,
    Ready
}

/// A connection to a MSSQL server

pub trait TargetStream: Read + Write + fmt::Debug {}
impl<T: Read + Write + fmt::Debug> TargetStream for T {}

pub struct Connection<'a, S: 'a + TargetStream>(Rc<RefCell<InternalConnection<'a, S>>>);

#[derive(Debug)]
pub enum AuthenticationMethod<'a> {
    /// username, password
    InternalSqlServerAuth(Cow<'a, str>, Cow<'a, str>)
}

impl<'a> AuthenticationMethod<'a> {
    pub fn internal<U: Into<Cow<'a, str>>, P: Into<Cow<'a, str>>>(username: U, password: P) -> AuthenticationMethod<'a> {
        AuthenticationMethod::InternalSqlServerAuth(username.into(), password.into())
    }
}

pub struct ConnectionOptBuilder<'a, S: 'a + TargetStream> {
    auth: Option<AuthenticationMethod<'a>>,
    database: Option<Cow<'a, str>>,
    stream: S,
}

impl<'a, S: 'a + TargetStream> ConnectionOptBuilder<'a, S> {
    pub fn new(stream: S) -> ConnectionOptBuilder<'a, S> {
        ConnectionOptBuilder {
            auth: None,
            database: None,
            stream: stream,
        }
    }
    pub fn auth(mut self, method: AuthenticationMethod<'a>) -> ConnectionOptBuilder<'a, S> {
        self.auth = Some(method);
        self
    }

    pub fn db<D: Into<Cow<'a, str>>>(mut self, db: D) -> ConnectionOptBuilder<'a, S> {
        self.database = Some(db.into());
        self
    }

    pub fn build(self) -> ConnectionOptions<'a, S> {
        ConnectionOptions {
            auth: self.auth.unwrap(),
            database: self.database.unwrap(),
            stream: self.stream,
        }
    }
}

#[derive(Debug)]
pub struct ConnectionOptions<'a, S: 'a + TargetStream> {
    pub auth: AuthenticationMethod<'a>,
    pub database: Cow<'a, str>,
    pub stream: S,
}

pub trait IntoConnectOpts<'a, S: 'a + TargetStream> {
    fn into_connect_opts(self) -> TdsResult<ConnectionOptions<'a, S>>;
}

impl<'a, S: 'a + TargetStream> IntoConnectOpts<'a, S> for ConnectionOptions<'a, S> {
    fn into_connect_opts(self) -> TdsResult<ConnectionOptions<'a, S>> {
        Ok(self)
    }
}

/// allow construction connection options by using a ODBC connection string
/// as specified in "ODBC Driver Connection String Keywords"
/// https://msdn.microsoft.com/de-de/library/ms130822(v=sql.120).aspx
///
/// supported options: Server, Database, UID, PWD
///
/// a sample connection string could be something like:
/// `Server=localhost;Database=testdb;UID=test;PWD=1234`
impl <'a> IntoConnectOpts<'a, Box<TargetStream>> for &'a str {
    fn into_connect_opts(self) -> TdsResult<ConnectionOptions<'a, Box<TargetStream>>> {
        struct ParsedContext<'a> {
            auth_method: Option<AuthenticationMethod<'a>>,
            db: Option<Cow<'a, str>>
        }

        fn apply_opts<'a>(ctxt: Box<ParsedContext<'a>>, mut opts_builder: ConnectionOptBuilder<'a, Box<TargetStream>>) -> ConnectionOptions<'a, Box<TargetStream>> {
            if let Some(ref x) = ctxt.db {
                opts_builder = opts_builder.db(x.clone());
            }
            if let Some(x) = ctxt.auth_method {
                opts_builder = opts_builder.auth(x);
            }
            opts_builder.build()
        }

        let mut ctxt = ParsedContext {
            auth_method: None,
            db: None
        };
        let mut builder = None;

        for opt in self.split(";") {
            let parts: Vec<&str> = opt.splitn(2, "=").collect();
            assert_eq!(parts.len(), 2);
            match &parts[0].to_lowercase()[..] {
                "uid" => {
                    ctxt.auth_method = match ctxt.auth_method {
                        Some(AuthenticationMethod::InternalSqlServerAuth(_, p)) => Some(AuthenticationMethod::internal(parts[1], p)),
                        _ => Some(AuthenticationMethod::internal(parts[1], ""))
                    }
                },
                "pwd" => {
                    ctxt.auth_method = match ctxt.auth_method {
                        Some(AuthenticationMethod::InternalSqlServerAuth(u, _)) => Some(AuthenticationMethod::internal(u, parts[1])),
                        _ => Some(AuthenticationMethod::internal("", parts[1]))
                    }
                },
                "database" => ctxt.db = Some(Cow::Borrowed(parts[1])),
                "server" => {
                    let stream = try!(TcpStream::connect(parts[1]));
                    builder = Some(ConnectionOptBuilder::new(Box::new(stream) as Box<TargetStream>));
                },
                _ => panic!("TODO! unknown parameter {}", parts[0])
            }
        }
        if let Some(x) = builder {
            return Ok(apply_opts(Box::new(ctxt), x))
        }
        Err(TdsError::Other("server not specified".to_owned()))
    }
}

// manual impl since autoderef seemed to mess up when cloning
impl<'a, S: 'a + TargetStream> Connection<'a, S> {
    pub fn clone(&'a self) -> Connection<'a, S> {
        Connection(self.0.clone())
    }
}

impl<'c, S: 'c + TargetStream> Connection<'c, S> {
    /// Execute the given query and return the resulting rows
    pub fn query<L>(&'c self, sql: L) -> TdsResult<QueryResult> where L: Into<Cow<'c, str>> {
        let stmt = StatementInternal::new(self.clone(), sql.into());
        Ok(try!(stmt.execute_into_query()))
    }

    /// Execute a sql statement and return the number of affected rows
    pub fn exec<L>(&'c self, sql: L) -> TdsResult<usize> where L: Into<Cow<'c, str>> {
        let mut stmt = StatementInternal::new(self.clone(), sql.into());
        Ok(try!(stmt.execute()))
    }

    pub fn prepare<L>(&'c self, sql: L) -> TdsResult<PreparedStatement<'c, S>> where L: Into<Cow<'c, str>> {
        Ok(try!(PreparedStatement::new(self.clone(), sql.into())))
    }

    /// Creates a parameterized SQL statement to be used with `sp_executesql`
    /// See [sp_executesql](https://msdn.microsoft.com/en-us/library/ms188001.aspx)
    /// for additional information.
    pub fn parameterized<L, K>(&'c self,
                               sql: L,
                               parameter_meta: K)
                               -> TdsResult<ParameterizedStatement<'c, S>>
        where L: Into<Cow<'c, str>>, K: Into<Option<Cow<'c, str>>>
    {
        Ok(try!(ParameterizedStatement::new(self.clone(),
                                            sql.into(),
                                            parameter_meta.into())))
    }
}

impl<'a, S: 'a + TargetStream> Deref for Connection<'a, S> {
    type Target = Rc<RefCell<InternalConnection<'a, S>>>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<'a, S: 'a + TargetStream> Connection<'a, S> {
    pub fn connect<T: IntoConnectOpts<'a, S>>(opts: T) -> TdsResult<Connection<'a, S>> {
        let opts = try!(opts.into_connect_opts());
        let mut conn = InternalConnection::new(opts);
        try!(conn.initialize());
        Ok(Connection(Rc::new(RefCell::new(conn))))
    }
}

pub struct TcpConnectionBuilder;
impl TcpConnectionBuilder {
    /// connects to the SQL server using the TCP protocol and returns get a config builder for the connection
    pub fn new_connect<'a, A: ToSocketAddrs>(addrs: A) -> TdsResult<ConnectionOptBuilder<'a, TcpStream>> {
        Ok(ConnectionOptBuilder::new(try!(TcpStream::connect(addrs))))
    }
}

/// Internal representation of a Internal Connection
#[doc(hidden)]
pub struct InternalConnection<'a, S: 'a + TargetStream> {
    pub state: ClientState,
    last_packet_id: u8,
    pub opts: ConnectionOptions<'a, S>,
    packet_size: u16,
}

impl<'c, S: 'c + TargetStream> InternalConnection<'c, S> {
    fn new(opts: ConnectionOptions<'c, S>) -> InternalConnection<'c, S> {
        InternalConnection {
            state: ClientState::Initial,
            last_packet_id: 0,
            opts: opts,
            packet_size: 0x1000,
        }
    }

    #[inline]
    fn alloc_id(&mut self) -> u8 {
        let id = self.last_packet_id;
        self.last_packet_id = (id + 1) % 255;
        id
    }

    /// Send a prelogin packet with version number 9.0.0000 (>=TDS 7.3 ?), and US_SUBBUILD=0 (for MSSQL always 0)
    fn initialize(&mut self) -> TdsResult<()> {
        try!(self.send_packet(&Packet::PreLogin(vec![
            OptionTokenPair::Version(0x09000000, 0),
            OptionTokenPair::Encryption(EncryptionSetting::NotSupported),
            OptionTokenPair::Instance("".to_owned()),
            OptionTokenPair::ThreadId(0),
            OptionTokenPair::Mars(0)
        ])));
        {
            let response_packet = try!(self.read_packet());
            // TODO: move catch_error and tokenstream env change handling into one general "generic handle" func?
            try!(response_packet.catch_error());
        }
        self.state = ClientState::PreloginPerformed;
        let mut login_packet = Login7::new(0x03000A73);
        {
            login_packet.set_auth(&self.opts.auth);
            login_packet.set_db(self.opts.database.clone());
            login_packet.packet_size = self.packet_size as u32;
        }
        let packet = Packet::Login(login_packet);
        try!(self.send_packet(&packet));
        {
            let response_packet = try!(self.read_packet());
            try!(response_packet.catch_error());
            match response_packet {
                Packet::TokenStream(tokens) => {
                    for token in tokens {
                        match token {
                            TokenStream::EnvChange(TokenStreamEnvChange::PacketSize(x, _)) => {
                                self.packet_size = try!(x.parse::<u16>().map_err(|e| TdsError::Other(format!("cannot convert packet size: {:?}", e))));
                            },
                            _ => ()
                        }
                    }
                },
                _ => return Err(TdsError::Other("expected a envchange setting a packet size after the login".to_owned()))
            }
        }
        // TODO verify and use response data
        self.state = ClientState::Ready;
        Ok(())
    }

    #[inline]
    pub fn internal_exec(&mut self, sql: &str) -> TdsResult<()> {
        assert_eq!(self.state, ClientState::Ready);
        try!(self.send_packet(&Packet::SqlBatch(sql)));
        Ok(())
    }

    /// read and parse "simple" packets
    pub fn read_packet<'a>(&mut self) -> TdsResult<Packet<'a>> {
        let packet = try!(self.opts.stream.read_message());
        Ok(match self.state {
            ClientState::Initial => {
                try!(packet.into_prelogin())
            },
            ClientState::PreloginPerformed => {
                try!(packet.into_general_token_stream())
            },
            ClientState::Ready => {
                panic!("read_packet: cannot be used in ready state");
            }
        })
    }

    /// Convert a message-packet into a protocol-packet
    /// ensure that packets are sent properly, respecting the
    /// configured `max packet size` and allocate
    /// a packet-id for each sent packet
    pub fn send_packet(&mut self, packet: &Packet) -> TdsResult<()> {
        let mut header = PacketHeader::new();
        let mut packet = try!(self.opts.stream.build_packet(header, packet));
        // if we don't have to split the packet due to max packet size, sent it
        if packet.header.length < self.packet_size {
            header.id = self.alloc_id();
            try!(self.opts.stream.write_packet(&mut packet));
            return Ok(())
        }
        packet.header.status = PacketStatus::NormalMessage;
        while !packet.data.is_empty() {
            let next_data = if self.packet_size as usize > packet.data.len() + packets::HEADER_SIZE as usize {
                    packet.header.status = PacketStatus::EndOfMessage;
                    vec![]
            } else {
                let idx = (self.packet_size - packets::HEADER_SIZE) as usize;
                let mut current = packet.data;
                let next = current.split_off(idx);
                packet.data = current;
                next
            };
            packet.header.id = self.alloc_id();
            packet.update_len();
            try!(self.opts.stream.write_packet(&mut packet));
            packet.data = next_data;
        }
        Ok(())
    }
}
