extern crate chrono;
extern crate msql_srv;
extern crate mysql;
extern crate mysql_common as myc;
extern crate nom;

use mysql::prelude::*;
use mysql::DriverError;
use mysql::OptsBuilder;
use mysql::SslOpts;
#[cfg(feature = "tls")]
use rustls::{Certificate, PrivateKey, ServerConfig};
use std::error::Error;
use std::io;
use std::net;
use std::thread;

use msql_srv::{
    Column, ErrorKind, InitWriter, MysqlIntermediary, MysqlShim, ParamParser, QueryResultWriter,
    StatementMetaWriter,
};

struct TestingShim<Q, P, E, I> {
    columns: Vec<Column>,
    params: Vec<Column>,
    on_q: Q,
    on_p: P,
    on_e: E,
    on_i: I,
    #[cfg(feature = "tls")]
    server_tls: Option<std::sync::Arc<rustls::ServerConfig>>,
    client_tls: Option<SslOpts>,
}

impl<Q, P, E, I> MysqlShim<net::TcpStream> for TestingShim<Q, P, E, I>
where
    Q: FnMut(&str, QueryResultWriter<net::TcpStream>) -> io::Result<()>,
    P: FnMut(&str) -> u32,
    E: FnMut(u32, Vec<msql_srv::ParamValue>, QueryResultWriter<net::TcpStream>) -> io::Result<()>,
    I: FnMut(&str, InitWriter<net::TcpStream>) -> io::Result<()>,
{
    type Error = io::Error;

    fn on_prepare(
        &mut self,
        query: &str,
        info: StatementMetaWriter<net::TcpStream>,
    ) -> io::Result<()> {
        let id = (self.on_p)(query);
        info.reply(id, &self.params, &self.columns)
    }

    fn on_execute(
        &mut self,
        id: u32,
        params: ParamParser,
        results: QueryResultWriter<net::TcpStream>,
    ) -> io::Result<()> {
        (self.on_e)(id, params.into_iter().collect(), results)
    }

    fn on_close(&mut self, _: u32) {}

    fn on_init(&mut self, schema: &str, writer: InitWriter<net::TcpStream>) -> io::Result<()> {
        (self.on_i)(schema, writer)
    }

    fn on_query(
        &mut self,
        query: &str,
        results: QueryResultWriter<net::TcpStream>,
    ) -> io::Result<()> {
        (self.on_q)(query, results)
    }

    #[cfg(feature = "tls")]
    fn tls_config(&self) -> Option<std::sync::Arc<rustls::ServerConfig>> {
        self.server_tls.as_ref().map(std::sync::Arc::clone)
    }
}

impl<Q, P, E, I> TestingShim<Q, P, E, I>
where
    Q: 'static + Send + FnMut(&str, QueryResultWriter<net::TcpStream>) -> io::Result<()>,
    P: 'static + Send + FnMut(&str) -> u32,
    E: 'static
        + Send
        + FnMut(u32, Vec<msql_srv::ParamValue>, QueryResultWriter<net::TcpStream>) -> io::Result<()>,
    I: 'static + Send + FnMut(&str, InitWriter<net::TcpStream>) -> io::Result<()>,
{
    fn new(on_q: Q, on_p: P, on_e: E, on_i: I) -> Self {
        TestingShim {
            columns: Vec::new(),
            params: Vec::new(),
            on_q,
            on_p,
            on_e,
            on_i,
            #[cfg(feature = "tls")]
            server_tls: None,
            client_tls: None,
        }
    }

    fn with_params(mut self, p: Vec<Column>) -> Self {
        self.params = p;
        self
    }

    fn with_columns(mut self, c: Vec<Column>) -> Self {
        self.columns = c;
        self
    }

    #[cfg(feature = "tls")]
    fn with_server_tls(mut self) -> Self {
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();

        self.server_tls = Some(std::sync::Arc::new(
            ServerConfig::builder()
                .with_safe_defaults()
                .with_no_client_auth()
                .with_single_cert(
                    vec![Certificate(cert.serialize_der().unwrap())],
                    PrivateKey(cert.get_key_pair().serialize_der()),
                )
                .unwrap(),
        ));

        self
    }

    fn with_client_tls(mut self) -> Self {
        self.client_tls = Some(SslOpts::default().with_danger_accept_invalid_certs(true));
        self
    }

    fn test<C>(self, c: C)
    where
        C: FnOnce(&mut mysql::Conn),
    {
        self.test_with_result(c).unwrap()
    }

    fn test_with_result<C>(self, c: C) -> Result<(), Box<dyn Error + 'static>>
    where
        C: FnOnce(&mut mysql::Conn) -> (),
    {
        let client_tls = self.client_tls.clone();

        let listener = net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let jh = thread::spawn(move || {
            let (s, _) = listener.accept().unwrap();
            MysqlIntermediary::run_on_tcp(self, s)
        });

        let opts = OptsBuilder::default()
            .ip_or_hostname(Some("localhost"))
            .tcp_port(port)
            .ssl_opts(client_tls);

        let mut db = mysql::Conn::new(opts)?;

        c(&mut db);
        drop(db);
        jh.join().unwrap().unwrap();

        Ok(())
    }
}

#[test]
fn it_connects() {
    TestingShim::new(
        |_, _| unreachable!(),
        |_| unreachable!(),
        |_, _, _| unreachable!(),
        |_, _| unreachable!(),
    )
    .test(|_| {});
}

#[test]
#[cfg(feature = "tls")]
fn it_connects_tls_server_only() {
    // Client can connect ok without SSL when SSL is enabled on the server.
    TestingShim::new(
        |_, _| unreachable!(),
        |_| unreachable!(),
        |_, _, _| unreachable!(),
        |_, _| unreachable!(),
    )
    .with_server_tls()
    .test(|_| {})
}

#[test]
#[cfg(feature = "tls")]
fn it_connects_tls_both() {
    // SSL connection when ssl enabled on server and used by client
    TestingShim::new(
        |_, _| unreachable!(),
        |_| unreachable!(),
        |_, _, _| unreachable!(),
        |_, _| unreachable!(),
    )
    .with_server_tls()
    .with_client_tls()
    .test(|_| {})
}

#[test]
fn it_does_not_connect_tls_client_only() {
    // Client requesting tls fails as expected when server does not support it.

    let e = TestingShim::new(
        |_, _| unreachable!(),
        |_| unreachable!(),
        |_, _, _| unreachable!(),
        |_, _| unreachable!(),
    )
    .with_client_tls()
    .test_with_result(|_| {})
    .expect_err("client should not have connected");

    assert!(
        matches!(
            e.downcast_ref::<mysql::Error>(),
            Some(mysql::Error::DriverError(DriverError::TlsNotSupported))
        ),
        "unexpected error {:?}",
        e
    );
}

#[test]
fn it_inits_ok() {
    TestingShim::new(
        |_, _| unreachable!(),
        |_| unreachable!(),
        |_, _, _| unreachable!(),
        |schema, writer| {
            assert_eq!(schema, "test");
            writer.ok()
        },
    )
    .test(|db| assert_eq!(true, db.select_db("test")));
}

#[test]
fn it_inits_error() {
    TestingShim::new(
        |_, _| unreachable!(),
        |_| unreachable!(),
        |_, _, _| unreachable!(),
        |schema, writer| {
            assert_eq!(schema, "test");
            writer.error(
                ErrorKind::ER_BAD_DB_ERROR,
                format!("Database {} not found", schema).as_bytes(),
            )
        },
    )
    .test(|db| assert_eq!(false, db.select_db("test")));
}

#[test]
fn it_inits_on_use_query_ok() {
    TestingShim::new(
        |_, _| unreachable!(),
        |_| unreachable!(),
        |_, _, _| unreachable!(),
        |schema, writer| {
            assert_eq!(schema, "test");
            writer.ok()
        },
    )
    .test(|db| match db.query_drop("USE `test`;") {
        Ok(_) => assert!(true),
        Err(_) => assert!(false),
    });
}

#[test]
fn it_pings() {
    TestingShim::new(
        |_, _| unreachable!(),
        |_| unreachable!(),
        |_, _, _| unreachable!(),
        |_, _| unreachable!(),
    )
    .test(|db| assert_eq!(db.ping(), true))
}

#[test]
fn empty_response() {
    TestingShim::new(
        |_, w| w.completed(0, 0),
        |_| unreachable!(),
        |_, _, _| unreachable!(),
        |_, _| unreachable!(),
    )
    .test(|db| {
        assert_eq!(db.query_iter("SELECT a, b FROM foo").unwrap().count(), 0);
    })
}

#[test]
fn no_rows() {
    let cols = [Column {
        table: String::new(),
        column: "a".to_owned(),
        coltype: myc::constants::ColumnType::MYSQL_TYPE_SHORT,
        colflags: myc::constants::ColumnFlags::empty(),
    }];
    TestingShim::new(
        move |_, w| w.start(&cols[..])?.finish(),
        |_| unreachable!(),
        |_, _, _| unreachable!(),
        |_, _| unreachable!(),
    )
    .test(|db| {
        assert_eq!(db.query_iter("SELECT a, b FROM foo").unwrap().count(), 0);
    })
}

#[test]
fn no_columns() {
    TestingShim::new(
        move |_, w| w.start(&[])?.finish(),
        |_| unreachable!(),
        |_, _, _| unreachable!(),
        |_, _| unreachable!(),
    )
    .test(|db| {
        assert_eq!(db.query_iter("SELECT a, b FROM foo").unwrap().count(), 0);
    })
}

#[test]
fn no_columns_but_rows() {
    TestingShim::new(
        move |_, w| w.start(&[])?.write_col(42).map(|_| ()),
        |_| unreachable!(),
        |_, _, _| unreachable!(),
        |_, _| unreachable!(),
    )
    .test(|db| {
        assert_eq!(db.query_iter("SELECT a, b FROM foo").unwrap().count(), 0);
    })
}

#[test]
fn error_response() {
    let err = (ErrorKind::ER_NO, "clearly not");
    TestingShim::new(
        move |_, w| w.error(err.0, err.1.as_bytes()),
        |_| unreachable!(),
        |_, _, _| unreachable!(),
        |_, _| unreachable!(),
    )
    .test(|db| {
        if let mysql::Error::MySqlError(e) = db.query_iter("SELECT a, b FROM foo").unwrap_err() {
            assert_eq!(
                e,
                mysql::error::MySqlError {
                    state: String::from_utf8(err.0.sqlstate().to_vec()).unwrap(),
                    message: err.1.to_owned(),
                    code: err.0 as u16,
                }
            );
        } else {
            unreachable!();
        }
    })
}

#[test]
fn error_in_result_set_response() {
    let err = (ErrorKind::ER_NO, "clearly not");
    TestingShim::new(
        move |_, w| {
            let cols = &[Column {
                table: String::new(),
                column: "a".to_owned(),
                coltype: myc::constants::ColumnType::MYSQL_TYPE_SHORT,
                colflags: myc::constants::ColumnFlags::empty(),
            }];
            let mut w = w.start(cols)?;
            w.write_col(1024)?;
            w.finish_error(err.0, &err.1.as_bytes())
        },
        |_| unreachable!(),
        |_, _, _| unreachable!(),
        |_, _| unreachable!(),
    )
    .test(|db| {
        let mut result = db.query_iter("SELECT a FROM foo").unwrap();
        let row1 = result.next().unwrap().unwrap().get::<i16, _>(0).unwrap();
        assert_eq!(row1, 1024);
        if let mysql::Error::MySqlError(e) = result.by_ref().next().unwrap().unwrap_err() {
            assert_eq!(
                e,
                mysql::error::MySqlError {
                    state: String::from_utf8(err.0.sqlstate().to_vec()).unwrap(),
                    message: err.1.to_owned(),
                    code: err.0 as u16,
                }
            );
        } else {
            unreachable!()
        }
    })
}

#[test]
fn empty_on_drop() {
    let cols = [Column {
        table: String::new(),
        column: "a".to_owned(),
        coltype: myc::constants::ColumnType::MYSQL_TYPE_SHORT,
        colflags: myc::constants::ColumnFlags::empty(),
    }];
    TestingShim::new(
        move |_, w| w.start(&cols[..]).map(|_| ()),
        |_| unreachable!(),
        |_, _, _| unreachable!(),
        |_, _| unreachable!(),
    )
    .test(|db| {
        assert_eq!(db.query_iter("SELECT a, b FROM foo").unwrap().count(), 0);
    })
}

#[test]
fn it_queries_nulls() {
    TestingShim::new(
        |_, w| {
            let cols = &[Column {
                table: String::new(),
                column: "a".to_owned(),
                coltype: myc::constants::ColumnType::MYSQL_TYPE_SHORT,
                colflags: myc::constants::ColumnFlags::empty(),
            }];
            let mut w = w.start(cols)?;
            w.write_col(None::<i16>)?;
            w.finish()
        },
        |_| unreachable!(),
        |_, _, _| unreachable!(),
        |_, _| unreachable!(),
    )
    .test(|db| {
        let row = db
            .query_iter("SELECT a, b FROM foo")
            .unwrap()
            .next()
            .unwrap()
            .unwrap();
        assert_eq!(row.as_ref(0), Some(&mysql::Value::NULL));
    })
}

#[test]
fn it_queries() {
    TestingShim::new(
        |_, w| {
            let cols = &[Column {
                table: String::new(),
                column: "a".to_owned(),
                coltype: myc::constants::ColumnType::MYSQL_TYPE_SHORT,
                colflags: myc::constants::ColumnFlags::empty(),
            }];
            let mut w = w.start(cols)?;
            w.write_col(1024i16)?;
            w.finish()
        },
        |_| unreachable!(),
        |_, _, _| unreachable!(),
        |_, _| unreachable!(),
    )
    .test(|db| {
        let row = db
            .query_iter("SELECT a, b FROM foo")
            .unwrap()
            .next()
            .unwrap()
            .unwrap();
        assert_eq!(row.get::<i16, _>(0), Some(1024));
    })
}

#[test]
fn multi_result() {
    TestingShim::new(
        |_, w| {
            let cols = &[Column {
                table: String::new(),
                column: "a".to_owned(),
                coltype: myc::constants::ColumnType::MYSQL_TYPE_SHORT,
                colflags: myc::constants::ColumnFlags::empty(),
            }];
            let mut row = w.start(cols)?;
            row.write_col(1024i16)?;
            let w = row.finish_one()?;
            let mut row = w.start(cols)?;
            row.write_col(1025i16)?;
            row.finish()
        },
        |_| unreachable!(),
        |_, _, _| unreachable!(),
        |_, _| unreachable!(),
    )
    .test(|db| {
        let mut result = db
            .query_iter("SELECT a FROM foo; SELECT a FROM foo")
            .unwrap();
        let mut set = result.iter().unwrap();
        let row1: Vec<_> = set
            .by_ref()
            .filter_map(|row| row.unwrap().get::<i16, _>(0))
            .collect();
        assert_eq!(row1, vec![1024]);
        drop(set);
        let mut set = result.iter().unwrap();
        let row2: Vec<_> = set
            .by_ref()
            .filter_map(|row| row.unwrap().get::<i16, _>(0))
            .collect();
        assert_eq!(row2, vec![1025]);
        drop(set);
        assert!(result.iter().is_none());
    })
}

#[test]
fn it_queries_many_rows() {
    TestingShim::new(
        |_, w| {
            let cols = &[
                Column {
                    table: String::new(),
                    column: "a".to_owned(),
                    coltype: myc::constants::ColumnType::MYSQL_TYPE_SHORT,
                    colflags: myc::constants::ColumnFlags::empty(),
                },
                Column {
                    table: String::new(),
                    column: "b".to_owned(),
                    coltype: myc::constants::ColumnType::MYSQL_TYPE_SHORT,
                    colflags: myc::constants::ColumnFlags::empty(),
                },
            ];
            let mut w = w.start(cols)?;
            w.write_col(1024i16)?;
            w.write_col(1025i16)?;
            w.end_row()?;
            w.write_row(&[1024i16, 1025i16])?;
            w.finish()
        },
        |_| unreachable!(),
        |_, _, _| unreachable!(),
        |_, _| unreachable!(),
    )
    .test(|db| {
        let mut rows = 0;
        for row in db.query_iter("SELECT a, b FROM foo").unwrap() {
            let row = row.unwrap();
            assert_eq!(row.get::<i16, _>(0), Some(1024));
            assert_eq!(row.get::<i16, _>(1), Some(1025));
            rows += 1;
        }
        assert_eq!(rows, 2);
    })
}

#[test]
fn it_prepares() {
    let cols = vec![Column {
        table: String::new(),
        column: "a".to_owned(),
        coltype: myc::constants::ColumnType::MYSQL_TYPE_SHORT,
        colflags: myc::constants::ColumnFlags::empty(),
    }];
    let cols2 = cols.clone();
    let params = vec![Column {
        table: String::new(),
        column: "c".to_owned(),
        coltype: myc::constants::ColumnType::MYSQL_TYPE_SHORT,
        colflags: myc::constants::ColumnFlags::empty(),
    }];

    TestingShim::new(
        |_, _| unreachable!(),
        |q| {
            assert_eq!(q, "SELECT a FROM b WHERE c = ?");
            41
        },
        move |stmt, params, w| {
            assert_eq!(stmt, 41);
            assert_eq!(params.len(), 1);
            // rust-mysql sends all numbers as LONGLONG
            assert_eq!(
                params[0].coltype,
                myc::constants::ColumnType::MYSQL_TYPE_LONGLONG
            );
            assert_eq!(Into::<i8>::into(params[0].value), 42i8);

            let mut w = w.start(&cols)?;
            w.write_col(1024i16)?;
            w.finish()
        },
        |_, _| unreachable!(),
    )
    .with_params(params)
    .with_columns(cols2)
    .test(|db| {
        let row = db
            .exec_iter("SELECT a FROM b WHERE c = ?", (42i16,))
            .unwrap()
            .next()
            .unwrap()
            .unwrap();
        assert_eq!(row.get::<i16, _>(0), Some(1024i16));
    })
}

#[test]
fn insert_exec() {
    let params = vec![
        Column {
            table: String::new(),
            column: "username".to_owned(),
            coltype: myc::constants::ColumnType::MYSQL_TYPE_VARCHAR,
            colflags: myc::constants::ColumnFlags::empty(),
        },
        Column {
            table: String::new(),
            column: "email".to_owned(),
            coltype: myc::constants::ColumnType::MYSQL_TYPE_VARCHAR,
            colflags: myc::constants::ColumnFlags::empty(),
        },
        Column {
            table: String::new(),
            column: "pw".to_owned(),
            coltype: myc::constants::ColumnType::MYSQL_TYPE_VARCHAR,
            colflags: myc::constants::ColumnFlags::empty(),
        },
        Column {
            table: String::new(),
            column: "created".to_owned(),
            coltype: myc::constants::ColumnType::MYSQL_TYPE_DATETIME,
            colflags: myc::constants::ColumnFlags::empty(),
        },
        Column {
            table: String::new(),
            column: "session".to_owned(),
            coltype: myc::constants::ColumnType::MYSQL_TYPE_VARCHAR,
            colflags: myc::constants::ColumnFlags::empty(),
        },
        Column {
            table: String::new(),
            column: "rss".to_owned(),
            coltype: myc::constants::ColumnType::MYSQL_TYPE_VARCHAR,
            colflags: myc::constants::ColumnFlags::empty(),
        },
        Column {
            table: String::new(),
            column: "mail".to_owned(),
            coltype: myc::constants::ColumnType::MYSQL_TYPE_VARCHAR,
            colflags: myc::constants::ColumnFlags::empty(),
        },
    ];

    TestingShim::new(
        |_, _| unreachable!(),
        |_| 1,
        move |_, params, w| {
            assert_eq!(params.len(), 7);
            assert_eq!(
                params[0].coltype,
                myc::constants::ColumnType::MYSQL_TYPE_VAR_STRING
            );
            assert_eq!(
                params[1].coltype,
                myc::constants::ColumnType::MYSQL_TYPE_VAR_STRING
            );
            assert_eq!(
                params[2].coltype,
                myc::constants::ColumnType::MYSQL_TYPE_VAR_STRING
            );
            assert_eq!(
                params[3].coltype,
                myc::constants::ColumnType::MYSQL_TYPE_DATETIME
            );
            assert_eq!(
                params[4].coltype,
                myc::constants::ColumnType::MYSQL_TYPE_VAR_STRING
            );
            assert_eq!(
                params[5].coltype,
                myc::constants::ColumnType::MYSQL_TYPE_VAR_STRING
            );
            assert_eq!(
                params[6].coltype,
                myc::constants::ColumnType::MYSQL_TYPE_VAR_STRING
            );
            assert_eq!(Into::<&str>::into(params[0].value), "user199");
            assert_eq!(Into::<&str>::into(params[1].value), "user199@example.com");
            assert_eq!(
                Into::<&str>::into(params[2].value),
                "$2a$10$Tq3wrGeC0xtgzuxqOlc3v.07VTUvxvwI70kuoVihoO2cE5qj7ooka"
            );
            assert_eq!(
                Into::<chrono::NaiveDateTime>::into(params[3].value),
                chrono::NaiveDate::from_ymd(2018, 4, 6).and_hms(13, 0, 56)
            );
            assert_eq!(Into::<&str>::into(params[4].value), "token199");
            assert_eq!(Into::<&str>::into(params[5].value), "rsstoken199");
            assert_eq!(Into::<&str>::into(params[6].value), "mtok199");

            w.completed(42, 1)
        },
        |_, _| unreachable!(),
    )
    .with_params(params)
    .test(|db| {
        let res = db
            .exec_iter(
                "INSERT INTO `users` \
                 (`username`, `email`, `password_digest`, `created_at`, \
                 `session_token`, `rss_token`, `mailing_list_token`) \
                 VALUES (?, ?, ?, ?, ?, ?, ?)",
                (
                    "user199",
                    "user199@example.com",
                    "$2a$10$Tq3wrGeC0xtgzuxqOlc3v.07VTUvxvwI70kuoVihoO2cE5qj7ooka",
                    mysql::Value::Date(2018, 4, 6, 13, 0, 56, 0),
                    "token199",
                    "rsstoken199",
                    "mtok199",
                ),
            )
            .unwrap();
        assert_eq!(res.affected_rows(), 42);
        assert_eq!(res.last_insert_id(), Some(1));
    })
}

#[test]
fn send_long() {
    let cols = vec![Column {
        table: String::new(),
        column: "a".to_owned(),
        coltype: myc::constants::ColumnType::MYSQL_TYPE_SHORT,
        colflags: myc::constants::ColumnFlags::empty(),
    }];
    let cols2 = cols.clone();
    let params = vec![Column {
        table: String::new(),
        column: "c".to_owned(),
        coltype: myc::constants::ColumnType::MYSQL_TYPE_BLOB,
        colflags: myc::constants::ColumnFlags::empty(),
    }];

    TestingShim::new(
        |_, _| unreachable!(),
        |q| {
            assert_eq!(q, "SELECT a FROM b WHERE c = ?");
            41
        },
        move |stmt, params, w| {
            assert_eq!(stmt, 41);
            assert_eq!(params.len(), 1);
            // rust-mysql sends all strings as VAR_STRING
            assert_eq!(
                params[0].coltype,
                myc::constants::ColumnType::MYSQL_TYPE_VAR_STRING
            );
            assert_eq!(Into::<&[u8]>::into(params[0].value), b"Hello world");

            let mut w = w.start(&cols)?;
            w.write_col(1024i16)?;
            w.finish()
        },
        |_, _| unreachable!(),
    )
    .with_params(params)
    .with_columns(cols2)
    .test(|db| {
        let row = db
            .exec_iter("SELECT a FROM b WHERE c = ?", (b"Hello world",))
            .unwrap()
            .next()
            .unwrap()
            .unwrap();
        assert_eq!(row.get::<i16, _>(0), Some(1024i16));
    })
}

#[test]
fn it_prepares_many() {
    let cols = vec![
        Column {
            table: String::new(),
            column: "a".to_owned(),
            coltype: myc::constants::ColumnType::MYSQL_TYPE_SHORT,
            colflags: myc::constants::ColumnFlags::empty(),
        },
        Column {
            table: String::new(),
            column: "b".to_owned(),
            coltype: myc::constants::ColumnType::MYSQL_TYPE_SHORT,
            colflags: myc::constants::ColumnFlags::empty(),
        },
    ];
    let cols2 = cols.clone();

    TestingShim::new(
        |_, _| unreachable!(),
        |q| {
            assert_eq!(q, "SELECT a, b FROM x");
            41
        },
        move |stmt, params, w| {
            assert_eq!(stmt, 41);
            assert_eq!(params.len(), 0);

            let mut w = w.start(&cols)?;
            w.write_col(1024i16)?;
            w.write_col(1025i16)?;
            w.end_row()?;
            w.write_row(&[1024i16, 1025i16])?;
            w.finish()
        },
        |_, _| unreachable!(),
    )
    .with_params(Vec::new())
    .with_columns(cols2)
    .test(|db| {
        let mut rows = 0;
        for row in db.exec_iter("SELECT a, b FROM x", ()).unwrap() {
            let row = row.unwrap();
            assert_eq!(row.get::<i16, _>(0), Some(1024));
            assert_eq!(row.get::<i16, _>(1), Some(1025));
            rows += 1;
        }
        assert_eq!(rows, 2);
    })
}

#[test]
fn prepared_empty() {
    let cols = vec![Column {
        table: String::new(),
        column: "a".to_owned(),
        coltype: myc::constants::ColumnType::MYSQL_TYPE_SHORT,
        colflags: myc::constants::ColumnFlags::empty(),
    }];
    let cols2 = cols;
    let params = vec![Column {
        table: String::new(),
        column: "c".to_owned(),
        coltype: myc::constants::ColumnType::MYSQL_TYPE_SHORT,
        colflags: myc::constants::ColumnFlags::empty(),
    }];

    TestingShim::new(
        |_, _| unreachable!(),
        |_| 0,
        move |_, params, w| {
            assert!(!params.is_empty());
            w.completed(0, 0)
        },
        |_, _| unreachable!(),
    )
    .with_params(params)
    .with_columns(cols2)
    .test(|db| {
        assert_eq!(
            db.exec_iter("SELECT a FROM b WHERE c = ?", (42i16,))
                .unwrap()
                .count(),
            0
        );
    })
}

#[test]
fn prepared_no_params() {
    let cols = vec![Column {
        table: String::new(),
        column: "a".to_owned(),
        coltype: myc::constants::ColumnType::MYSQL_TYPE_SHORT,
        colflags: myc::constants::ColumnFlags::empty(),
    }];
    let cols2 = cols.clone();
    let params = vec![];

    TestingShim::new(
        |_, _| unreachable!(),
        |_| 0,
        move |_, params, w| {
            assert!(params.is_empty());
            let mut w = w.start(&cols)?;
            w.write_col(1024i16)?;
            w.finish()
        },
        |_, _| unreachable!(),
    )
    .with_params(params)
    .with_columns(cols2)
    .test(|db| {
        let row = db.exec_iter("foo", ()).unwrap().next().unwrap().unwrap();
        assert_eq!(row.get::<i16, _>(0), Some(1024i16));
    })
}

#[test]
fn prepared_nulls() {
    let cols = vec![
        Column {
            table: String::new(),
            column: "a".to_owned(),
            coltype: myc::constants::ColumnType::MYSQL_TYPE_SHORT,
            colflags: myc::constants::ColumnFlags::empty(),
        },
        Column {
            table: String::new(),
            column: "b".to_owned(),
            coltype: myc::constants::ColumnType::MYSQL_TYPE_SHORT,
            colflags: myc::constants::ColumnFlags::empty(),
        },
    ];
    let cols2 = cols.clone();
    let params = vec![
        Column {
            table: String::new(),
            column: "c".to_owned(),
            coltype: myc::constants::ColumnType::MYSQL_TYPE_SHORT,
            colflags: myc::constants::ColumnFlags::empty(),
        },
        Column {
            table: String::new(),
            column: "d".to_owned(),
            coltype: myc::constants::ColumnType::MYSQL_TYPE_SHORT,
            colflags: myc::constants::ColumnFlags::empty(),
        },
    ];

    TestingShim::new(
        |_, _| unreachable!(),
        |_| 0,
        move |_, params, w| {
            assert_eq!(params.len(), 2);
            assert!(params[0].value.is_null());
            assert!(!params[1].value.is_null());
            assert_eq!(
                params[0].coltype,
                myc::constants::ColumnType::MYSQL_TYPE_NULL
            );
            // rust-mysql sends all numbers as LONGLONG :'(
            assert_eq!(
                params[1].coltype,
                myc::constants::ColumnType::MYSQL_TYPE_LONGLONG
            );
            assert_eq!(Into::<i8>::into(params[1].value), 42i8);

            let mut w = w.start(&cols)?;
            w.write_row(vec![None::<i16>, Some(42)])?;
            w.finish()
        },
        |_, _| unreachable!(),
    )
    .with_params(params)
    .with_columns(cols2)
    .test(|db| {
        let row = db
            .exec_iter(
                "SELECT a, b FROM x WHERE c = ? AND d = ?",
                (mysql::Value::NULL, 42),
            )
            .unwrap()
            .next()
            .unwrap()
            .unwrap();
        assert_eq!(row.as_ref(0), Some(&mysql::Value::NULL));
        assert_eq!(row.get::<i16, _>(1), Some(42));
    })
}

#[test]
fn prepared_no_rows() {
    let cols = vec![Column {
        table: String::new(),
        column: "a".to_owned(),
        coltype: myc::constants::ColumnType::MYSQL_TYPE_SHORT,
        colflags: myc::constants::ColumnFlags::empty(),
    }];
    let cols2 = cols.clone();
    TestingShim::new(
        |_, _| unreachable!(),
        |_| 0,
        move |_, _, w| w.start(&cols[..])?.finish(),
        |_, _| unreachable!(),
    )
    .with_columns(cols2)
    .test(|db| {
        assert_eq!(db.exec_iter("SELECT a, b FROM foo", ()).unwrap().count(), 0);
    })
}

#[test]
fn prepared_no_cols_but_rows() {
    TestingShim::new(
        |_, _| unreachable!(),
        |_| 0,
        move |_, _, w| w.start(&[])?.write_col(42).map(|_| ()),
        |_, _| unreachable!(),
    )
    .test(|db| {
        assert_eq!(db.exec_iter("SELECT a, b FROM foo", ()).unwrap().count(), 0);
    })
}

#[test]
fn prepared_no_cols() {
    TestingShim::new(
        |_, _| unreachable!(),
        |_| 0,
        move |_, _, w| w.start(&[])?.finish(),
        |_, _| unreachable!(),
    )
    .test(|db| {
        assert_eq!(db.exec_iter("SELECT a, b FROM foo", ()).unwrap().count(), 0);
    })
}

#[test]
fn really_long_query() {
    let long = "CREATE TABLE `stories` (`id` int unsigned NOT NULL AUTO_INCREMENT PRIMARY KEY, `always_null` int, `created_at` datetime, `user_id` int unsigned, `url` varchar(250) DEFAULT '', `title` varchar(150) DEFAULT '' NOT NULL, `description` mediumtext, `short_id` varchar(6) DEFAULT '' NOT NULL, `is_expired` tinyint(1) DEFAULT 0 NOT NULL, `is_moderated` tinyint(1) DEFAULT 0 NOT NULL, `markeddown_description` mediumtext, `story_cache` mediumtext, `merged_story_id` int, `unavailable_at` datetime, `twitter_id` varchar(20), `user_is_author` tinyint(1) DEFAULT 0,  INDEX `index_stories_on_created_at`  (`created_at`), fulltext INDEX `index_stories_on_description`  (`description`),   INDEX `is_idxes`  (`is_expired`, `is_moderated`),  INDEX `index_stories_on_is_expired`  (`is_expired`),  INDEX `index_stories_on_is_moderated`  (`is_moderated`),  INDEX `index_stories_on_merged_story_id`  (`merged_story_id`), UNIQUE INDEX `unique_short_id`  (`short_id`), fulltext INDEX `index_stories_on_story_cache`  (`story_cache`), fulltext INDEX `index_stories_on_title`  (`title`),  INDEX `index_stories_on_twitter_id`  (`twitter_id`),  INDEX `url`  (`url`(191)),  INDEX `index_stories_on_user_id`  (`user_id`)) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;";
    TestingShim::new(
        move |q, w| {
            assert_eq!(q, long);
            w.start(&[])?.finish()
        },
        |_| 0,
        |_, _, _| unreachable!(),
        |_, _| unreachable!(),
    )
    .test(move |db| {
        db.query_iter(long).unwrap();
    })
}
