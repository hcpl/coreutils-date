#![recursion_limit="128"]

extern crate chrono;
#[macro_use]
extern crate clap;
extern crate errno;
#[macro_use]
extern crate error_chain;
extern crate libc;
#[macro_use]
extern crate nom;

#[cfg(windows)]
extern crate kernel32;
#[cfg(windows)]
extern crate winapi;

/*
 * TODO
 * - make print and set argument groups mutually exclusive
 * - implement "(nearly) arbitrary text-to-datetime" parser
 * - print timezone abbrevations instead of UTC offsets
 */


use std::fs::{self, File};
use std::io::{self, Write, BufReader, BufRead};
use std::path::PathBuf;
use std::time;

use chrono::{DateTime, Offset, FixedOffset, Local, UTC, Datelike, Timelike, TimeZone};
use clap::{App, Arg, ArgGroup};
use errno::errno;
use nom::digit;


const DATE: &'static str = "date";
const HOURS: &'static str = "hours";
const MINUTES: &'static str = "minutes";
const SECONDS: &'static str = "seconds";
const NS: &'static str = "ns";


mod errors {
    use errno::Errno;
    use nom;

    error_chain! {
        foreign_links {
            Io(::std::io::Error);
            ChronoParse(::chrono::ParseError);
            SystemTime(::std::time::SystemTimeError);
        }

        errors {
            ParseMonth(month: u32) {
                description("parsing date month")
                display("parsing date month: '{}'", month)
            }

            ParseDay(day: u32) {
                description("parsing date day")
                display("parsing date day: '{}'", day)
            }

            ParseHour(hour: u32) {
                description("parsing date hour")
                display("parsing date hour: '{}'", hour)
            }

            ParseMinute(minute: u32) {
                description("parsing date minute")
                display("parsing date minute: '{}'", minute)
            }

            ParseYear(year: i32) {
                description("parsing date year")
                display("parsing date year: '{}'", year)
            }

            ParseSecond(second: u32) {
                description("parsing date second")
                display("parsing date second: '{}'", second)
            }

            Nom(e: nom::IError) {
                description("parsing error from nom")
                display("parsing error from nom: '{:?}'", e)
            }

            SetTime(errno: Errno) {
                description("cannot set time")
                display("cannot set time: '{}'", errno)
            }

            #[cfg(unix)]
            UnknownReturnCode(code: isize) {
                description("unknown return code")
                display("unknown return code: {}", code)
            }

            ArbitraryDateTimeParse(s: String) {
                description("cannot parse arbitrary datetime")
                display("cannot parse arbitrary datetime: '{}'", s)
            }
        }
    }

    impl From<nom::IError> for Error {
        fn from(e: nom::IError) -> Error {
            Error::from_kind(ErrorKind::Nom(e))
        }
    }
}


struct Settings {
    utc: bool,
    date_source: DateSource,
    format: Format,
    set_to: Option<DateTime<FixedOffset>>,
}

enum DateSource {
    Now,
    Custom(String),
    File(PathBuf),
    Reference(PathBuf),
}

enum Format {
    Iso8601(Iso8601Format),
    Rfc2822,
    Rfc3339(Rfc3339Format),
    Custom(String),
    Default,
}

enum Iso8601Format {
    Date,
    Hours,
    Minutes,
    Seconds,
    Ns,
}

enum Rfc3339Format {
    Date,
    Seconds,
    Ns,
}

impl<'a> From<&'a str> for Iso8601Format {
    fn from(s: &str) -> Self {
        match s {
            HOURS => Iso8601Format::Hours,
            MINUTES => Iso8601Format::Minutes,
            SECONDS => Iso8601Format::Seconds,
            NS => Iso8601Format::Ns,
            DATE => Iso8601Format::Date,
            // Should be caught by clap
            _ => panic!("Invalid format: {}", s),
        }
    }
}

impl<'a> From<&'a str> for Rfc3339Format {
    fn from(s: &str) -> Self {
        match s {
            DATE => Rfc3339Format::Date,
            SECONDS => Rfc3339Format::Seconds,
            NS => Rfc3339Format::Ns,
            // Should be caught by clap
            _ => panic!("Invalid format: {}", s),
        }
    }
}


pub fn uumain(args: Vec<String>) -> i32 {
    match uumain_impl(args) {
        Ok(()) => 0,
        Err(error) => {
            writeln!(io::stderr(), "{}", error).expect("couldn't write to stderr");
            1
        }
    }
}

fn uumain_impl(args: Vec<String>) -> errors::Result<()> {
    let settings = parse_cli(args)?;

    if let Some(date_time) = settings.set_to {
        set_time(&date_time)?;
    } else {
        let format_string = make_format_string(&settings.format);
        let print_date = |date: DateTime<FixedOffset>| {
            println!("{}", date.format(format_string));
        };

        match settings.date_source {
            DateSource::Custom(ref input) => {
                let date = input.parse()?;
                print_date(date)
            },
            DateSource::File(ref path) => {
                let file = File::open(path)?;
                for line in BufReader::new(file).lines() {
                    let date = line?.parse()?;
                    print_date(date);
                }
            },
            DateSource::Reference(ref path) => {
                let duration = fs::metadata(path)?.modified()?.duration_since(time::UNIX_EPOCH)?;
                let (secs, nsecs) = (duration.as_secs(), duration.subsec_nanos());
                let date = UTC.timestamp(secs as i64, nsecs);
                print_date(date.with_timezone(&date.offset().fix()))
            },
            DateSource::Now => {
                print_date(get_now(settings.utc))
            },
        };
    }

    Ok(())
}


fn parse_cli(args: Vec<String>) -> errors::Result<Settings> {
    let matches = App::new("date")
        .version(crate_version!())
        .about("Display the current time in the given FORMAT, or set the system date.")
        .arg(Arg::with_name("date")
                 .help("display time described by STRING, not 'now'")
                 .short("d")
                 .long("date")
                 .value_name("STRING"))
        .arg(Arg::with_name("file")
                 .help("like --date once for each line of DATEFILE")
                 .short("f")
                 .long("file")
                 .value_name("DATEFILE"))
        .arg(Arg::with_name("iso-8601")
                 .help("output date/time in ISO 8601 format. {n}\
                        TIMESPEC='date' for date only (the default), {n}\
                        'hours', 'minutes', 'seconds', or 'ns' for date {n}\
                        and time to the indicated precision.{n}")
                 .short("I")
                 .long("iso-8601")
                 .value_name("TIMESPEC")
                 .possible_values(&["date", "hours", "minutes", "seconds", "ns"]))
        .arg(Arg::with_name("reference")
                 .help("display the last modification time of FILE")
                 .short("r")
                 .long("reference")
                 .value_name("FILE"))
        .arg(Arg::with_name("rfc-2822")
                 .help("output date and time in RFC 2822 format. {n}\
                        Example: Mon, 07 Aug 2006 12:34:56 -0600")
                 .short("R")
                 .long("rfc-2822"))
        .arg(Arg::with_name("rfc-3339")
                 .help("output date and time in RFC 3339 format. {n}\
                        TIMESPEC='date', 'seconds', or 'ns' for {n}\
                        date and time to the indicated precision. {n}\
                        Date and time components are separated by {n}\
                        a single space: 2006-08-07 12:34:56-06:00{n}")
                 .long("rfc-3339")
                 .value_name("TIMESPEC")
                 .possible_values(&["date", "seconds", "ns"]))
        .arg(Arg::with_name("set")
                 .help("set time described by STRING")
                 .short("s")
                 .long("set")
                 .value_name("STRING"))
        .arg(Arg::with_name("utc")
                 .help("print or set Coordinated Universal Time")
                 .short("u")
                 .long("utc")
                 .long("universal"))
        .arg(Arg::with_name("format")
                 .value_name("+FORMAT")
                 .validator(|fmt| match fmt.starts_with('+') {
                     true  => Ok(()),
                     false => Err("Date formats must start with a '+' character".to_owned()),
                 }))
        .arg(Arg::with_name("positional set")
                 .value_name("MMDDhhmm[[CC]YY][.ss]"))
        .group(ArgGroup::with_name("print source")
                        .args(&["date", "file", "reference"]))
        .group(ArgGroup::with_name("format output")
                        .args(&["format", "iso-8601", "rfc-2822", "rfc-3339"]))
        .group(ArgGroup::with_name("set date")
                        .args(&["set", "positional set"])
                        .conflicts_with("print source"))
        .get_matches_from(args);

    let utc = matches.is_present("utc");

    let format = if let Some(fmt) = matches.value_of("format") {
        let fmt = fmt[1..].into();
        Format::Custom(fmt)
    } else if let Some(tspec) = matches.value_of("iso-8601") {
        Format::Iso8601(tspec.into())
    } else if matches.is_present("iso-8601") {
        Format::Iso8601(Iso8601Format::Date)
    } else if matches.is_present("iso-2822") {
        Format::Rfc2822
    } else if let Some(tspec) = matches.value_of("iso-3339") {
        Format::Rfc3339(tspec.into())
    } else {
        Format::Default
    };

    let date_source = if let Some(date) = matches.value_of("date") {
        DateSource::Custom(date.into())
    } else if let Some(file) = matches.value_of("file") {
        DateSource::File(file.into())
    } else if let Some(reference) = matches.value_of("reference") {
        DateSource::Reference(reference.into())
    } else {
        DateSource::Now
    };

    let set_to = if let Some(time) = matches.value_of("positional set") {
        Some(parse_custom_date_time(time, utc)?)
    } else if let Some(time) = matches.value_of("set") {
        Some(parse_date_time(time)?)
    } else {
        None
    };

    Ok(Settings {
        utc: utc,
        format: format,
        date_source: date_source,
        set_to: set_to,
    })
}

fn parse_custom_date_time(time: &str, utc: bool) -> errors::Result<DateTime<FixedOffset>> {
    named!(two_digits<&str, u32>, do_parse!(
        first: digit >>
        second: digit >>

        (first.parse::<u32>().expect("bbb") * 10 + second.parse::<u32>().expect("aaa"))
    ));

    use nom::IResult;
    let res: IResult<&str, DateTime<FixedOffset>> = do_parse!(time,
        month: two_digits >>
        day: two_digits >>
        hour: two_digits >>
        minute: two_digits >>
        before_dot1: opt!(two_digits) >>
        before_dot2: opt!(two_digits) >>
        second: opt!(do_parse!(
            char!('.') >> 
            digits: two_digits >>
            (digits)
        )) >>

        ({
            let date_time = get_now(utc);

            let year = before_dot1.map_or(date_time.year(), |yy| date_time.year() / 100 + yy as i32);
            let year = before_dot2.map_or(year, |yy| year / 10000 + yy as i32 * 100 + year % 100);
            let second = second.unwrap_or(0);

            date_time
                .with_month(month).ok_or(errors::ErrorKind::ParseMonth(month))?
                .with_day(day).ok_or(errors::ErrorKind::ParseDay(day))?
                .with_hour(hour).ok_or(errors::ErrorKind::ParseHour(hour))?
                .with_minute(minute).ok_or(errors::ErrorKind::ParseMinute(minute))?
                .with_year(year).ok_or(errors::ErrorKind::ParseYear(year))?
                .with_second(second).ok_or(errors::ErrorKind::ParseSecond(second))?
        })
    );

    Ok(res.to_full_result()?)
}

fn parse_date_time(time: &str) -> errors::Result<DateTime<FixedOffset>> {
    // TODO: Implement more conversion formats (and reorder to match that of GNU counterpart?)
    let parse_functions = [
        DateTime::<FixedOffset>::parse_from_rfc2822,
        DateTime::<FixedOffset>::parse_from_rfc3339];

    for parse in parse_functions.iter() {
        match parse(time) {
            Ok(date) => return Ok(date),
            Err(_)   => continue,
        }
    }

    Err(errors::ErrorKind::ArbitraryDateTimeParse(time.to_owned()).into())
}


#[cfg(not(windows))]
fn set_time(date_time: &DateTime<FixedOffset>) -> errors::Result<()> {
    // TODO: Double-check local vs UTC time
    use libc::{clock_settime, CLOCK_REALTIME, timespec, time_t, c_long};

    let tspec = timespec {
        tv_sec: date_time.timestamp() as time_t,
        tv_nsec: date_time.timestamp_subsec_nanos() as c_long,
    };

    let retcode;
    unsafe {
        retcode = clock_settime(CLOCK_REALTIME, &tspec as *const timespec);
    }

    match retcode {
        0  => Ok(()),
        -1 => Err(errors::ErrorKind::SetTime(errno()).into()),
        _  => Err(errors::ErrorKind::UnknownReturnCode(retcode as isize).into()),
    }
}

#[cfg(windows)]
#[allow(non_snake_case)]
fn set_time(date_time: &DateTime<FixedOffset>) -> errors::Result<()> {
    // TODO: Double-check local vs UTC time
    use kernel32::SetSystemTime;
    use winapi::minwinbase::SYSTEMTIME;
    use winapi::minwindef::{TRUE, FALSE, WORD};

    let lpSystemTime = SYSTEMTIME {
        wYear: date_time.year() as WORD,
        wMonth: date_time.month() as WORD,
        wDayOfWeek: date_time.weekday().num_days_from_sunday() as WORD,
        wDay: date_time.day() as WORD,
        wHour: date_time.hour() as WORD,
        wMinute: date_time.minute() as WORD,
        wSecond: date_time.second() as WORD,
        wMilliseconds: date_time.timestamp_subsec_millis() as WORD,
    };

    let retval;
    unsafe {
        retval = SetSystemTime(&lpSystemTime as *const SYSTEMTIME);
    }

    match retval {
        TRUE  => Ok(()),
        FALSE => Err(errors::ErrorKind::SetTime(errno()).into()),
        _     => unreachable!(),    // Well, reachable, if Windows kernel is crazy enough
    }
}


fn get_now(utc: bool) -> DateTime<FixedOffset> {
    if utc {
        let now = UTC::now();
        now.with_timezone(&now.offset().fix())
    } else {
        let now = Local::now();
        now.with_timezone(now.offset())
    }
}


fn make_format_string(format: &Format) -> &str {
    match format {
        &Format::Iso8601(ref fmt) => {
            match fmt {
                &Iso8601Format::Date => "%F",
                &Iso8601Format::Hours => "%FT%H%:z",
                &Iso8601Format::Minutes => "%FT%H:%M%:z",
                &Iso8601Format::Seconds => "%FT%T%:z",
                &Iso8601Format::Ns => "%FT%T,%f%:z",
            }
        }
        &Format::Rfc2822 => "%a, %d %h %Y %T %z",
        &Format::Rfc3339(ref fmt) => {
            match fmt {
                &Rfc3339Format::Date => "%F",
                &Rfc3339Format::Seconds => "%F %T%:z",
                &Rfc3339Format::Ns => "%F %T.%f%:z",
            }
        }
        &Format::Custom(ref fmt) => fmt,
        &Format::Default => "%a %b %e %T %Z %Y",
    }
}
