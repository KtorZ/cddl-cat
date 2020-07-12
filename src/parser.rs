//! This module contains a CDDL parser.
//!
//! The only public items here are the function [`parse_cddl`] and the error
//! [`ParseError`] and its child enum [`ErrorKind`].
//!
//! # Examples
//! ```
//! use cddl_cat::parse_cddl;
//!
//! let input = "map = { name: tstr }";
//! assert!(parse_cddl(input).is_ok());
//! ```

use crate::ast::*;
use escape8259::unescape;
use nom::{
    branch::alt,
    bytes::complete::{tag, take_while, take_while1},
    character::complete::{
        alpha0, anychar, char as charx, digit0, digit1, hex_digit1, multispace1, not_line_ending,
        one_of,
    },
    combinator::{all_consuming, map, map_res, opt, recognize, value as valuex},
    multi::{many0, many1, separated_nonempty_list},
    sequence::{delimited, pair, preceded, separated_pair, terminated, tuple},
};
use std::convert::TryFrom;
use std::error;
use std::fmt;

//
// A note on the design of the parser:
//
// Parsers built from the "nom" crate look a bit different from most other
// Rust code; for extra readability there is a lot of extra indents that
// rustfmt wouldn't like (and rustfmt::skip is applied extensively.)

// This error type is used everywhere in this parser.  It allows
// me to mix locally-generated custom errors with the errors that
// are automatically generated by the parser.
type JResult<I, O> = nom::IResult<I, O, ParseError>;

/// The "kind" of error generated during CDDL parsing.
#[rustversion::attr(since(1.40), non_exhaustive)]
#[derive(Debug, PartialEq)]
pub enum ErrorKind {
    /// An integer didn't parse correctly.
    MalformedInteger,
    /// A floating-point number didn't parse correctly.
    MalformedFloat,
    /// A hex literal didn't parse correctly.
    MalformedHex,
    /// A malformed text string
    MalformedText,
    /// A nonspecific parsing error.
    Unparseable,
}
use ErrorKind::*;

/// An error that occurred during CDDL parsing.
#[derive(Debug, PartialEq)]
pub struct ParseError {
    /// The "kind" of error generated during CDDL parsing.
    pub kind: ErrorKind,
    // FIXME: using String here is probably bad for performance because
    // lots of ephemeral errors will be created as each subparser is
    // attempted and then fails.  It would probably be smarter to allow
    // for &str contexts along the way.
    /// A snippet of text from the CDDL input that may be the cause of the error.
    pub ctx: String,
}

fn parse_error<S: Into<String>>(kind: ErrorKind, ctx: S) -> ParseError {
    ParseError {
        kind,
        ctx: ctx.into(),
    }
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:?}({})", self.kind, self.ctx)
    }
}

// Standard boilerplate, required so other errors can wrap this one.
impl error::Error for ParseError {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        None
    }
}

// Used when calling all_consuming() at the end of the parsing process.
impl From<nom::Err<ParseError>> for ParseError {
    fn from(e: nom::Err<ParseError>) -> ParseError {
        match e {
            nom::Err::Incomplete(_) => parse_error(Unparseable, "Incomplete"),
            nom::Err::Error(pe) => pe,
            nom::Err::Failure(pe) => pe,
        }
    }
}

// FIXME: the name collision here makes the code hard to read
impl<I: Into<String>> nom::error::ParseError<I> for ParseError {
    fn from_error_kind(input: I, _kind: nom::error::ErrorKind) -> Self {
        parse_error(Unparseable, input)
    }

    fn append(_input: I, _kind: nom::error::ErrorKind, other: Self) -> Self {
        // FIXME: It's not obvious what I should do here, or
        // when a proper implementation will be necessary...
        other
    }
}

// A workaround for the fact that nom::combinator::map_res discards the returned error type.
// See also https://github.com/Geal/nom/issues/1171
fn map_res_fail<I: Clone, O1, O2, E: nom::error::ParseError<I>, F, G>(
    first: F,
    second: G,
) -> impl Fn(I) -> nom::IResult<I, O2, E>
where
    F: Fn(I) -> nom::IResult<I, O1, E>,
    G: Fn(O1) -> Result<O2, E>,
{
    move |input: I| {
        let (input, o1) = first(input)?;
        match second(o1) {
            Ok(o2) => Ok((input, o2)),
            Err(e) => Err(nom::Err::Failure(e)),
        }
    }
}

// CDDL whitespace definition:
// Note no support for tabs, or naked linefeed characters.
//
// S = *WS
// WS = SP / NL
// SP = %x20
// NL = COMMENT / CRLF
// COMMENT = ";" *PCHAR CRLF
// PCHAR = %x20-7E / %x80-10FFFD
// CRLF = %x0A / %x0D.0A

// This varies a bit from the RFC, again, with respect to whitespace.
#[rustfmt::skip]
fn comment(input: &str) -> JResult<&str, &str> {
    // semicolon, anything, terminated by CR or CR+LF.
    preceded(
        charx(';'),
        not_line_ending
    )(input)
}

// Any amount of whitespace (including none) (including comments)
// Note: this counts tab characters as whitespace, which differs from RFC8610.
#[rustfmt::skip]
fn ws(input: &str) -> JResult<&str, &str> {
    recognize(
        many0(
            alt((
                // multispace1 includes tabs
                multispace1,
                comment
            ))
        )
    )
    (input)
}

#[test]
fn test_whitespace() {
    let cddl = "  ; a comment\n        \r\n; another;;;comment\n  ";
    let (remainder, _result) = ws(cddl).unwrap();
    assert_eq!(remainder, "");
}

// An optional comma and any whitespace surrounding it.
#[rustfmt::skip]
fn optcom(input: &str) -> JResult<&str, ()> {
    valuex((), pair(
        ws,
        opt(pair(
            tag(","),
            ws
        ))
    ))
    (input)
}

// id = EALPHA *(*("-" / ".") (EALPHA / DIGIT))
// So the first character needs to be EALPHA (alpha plus @_$)
// and then any number of EALPHA or DIGIT or "-" or ".",
// ending with EALPHA or DIGIT.
// we'll take this in steps:
// 1. first EALPHA char
// 2. do the following zero or more times:
//    a. optionally consume one of -.
//    b. consume EALPHA or DIGIT.

fn ealpha_char(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '@' || c == '_' || c == '$'
}

#[rustfmt::skip]
fn ealpha(input: &str) -> JResult<&str, &str> {
    take_while1(ealpha_char)
    (input)
}

// The tail characters of an ident: *("-" / ".") (EALPHA / DIGIT)
#[rustfmt::skip]
fn ident_tail(input: &str) -> JResult<&str, &str> {
    recognize(
        preceded(
            opt(many1(one_of("-."))),
            alt((
                ealpha,
                digit1
            ))
        )
    )
    (input)
}

#[rustfmt::skip]
fn ident(input: &str) -> JResult<&str, &str> {
    recognize(
        preceded(
            ealpha,
            many0(ident_tail)
        )
    )
    (input)
}

#[test]
fn test_ident() {
    assert_eq!(ident("a"), Ok(("", "a")));
    assert_eq!(ident("a1"), Ok(("", "a1")));
    assert_eq!(ident("a.1"), Ok(("", "a.1")));
    assert_eq!(ident("a1."), Ok((".", "a1")));
    assert_eq!(ident("@a1"), Ok(("", "@a1")));
    assert_eq!(ident("a..b"), Ok(("", "a..b")));
    assert!(ident("1a").is_err());
}

// uint = DIGIT1 *DIGIT
//      / "0x" 1*HEXDIG
//      / "0b" 1*BINDIG
//      / "0"
#[rustfmt::skip]
fn uint_hex(input: &str) -> JResult<&str, &str> {
    preceded(
        tag("0x"),
        hex_digit1
    )
    (input)
}

#[rustfmt::skip]
fn uint_binary(input: &str) -> JResult<&str, &str> {
    preceded(
        tag("0b"),
        recognize(many1(one_of("01")))
    )
    (input)
}

#[rustfmt::skip]
fn uint_decimal(input: &str) -> JResult<&str, &str> {
    alt((
        tag("0"),
        recognize(
            pair(
                one_of("123456789"),
                digit0
            )
        )
    ))
    (input)
}

// Parsing of integers, floats, etc. can get pretty complicated.  Because
// different downstream parsers may want the integer in different forms, we
// store its information in a temporary struct that preserves the original
// string slice along with some context that will help us remember what that
// string represents.
struct RawUint<'a> {
    slice: &'a str,
    base: u32,
}

// Parse the string for uint; return the slice and the intended base (radix).
#[rustfmt::skip]
fn uint(input: &str) -> JResult<&str, RawUint> {
    alt((
        map(uint_hex, |slice| RawUint{slice, base: 16}),
        map(uint_binary, |slice| RawUint{slice, base: 2}),
        map(uint_decimal, |slice| {
            RawUint{slice, base: 10}
        }),
    ))
    (input)
}

// Extract an unsigned integer using the correct base (radix).
#[rustfmt::skip]
fn uint_u64(input: &str) -> JResult<&str, u64> {
    map_res_fail(uint, |raw| {
        u64::from_str_radix(raw.slice, raw.base)
        .map_err(|_| {
            parse_error(MalformedInteger, raw.slice)
        })
    })
    (input)
}

#[test]
fn test_uint() {
    assert_eq!(uint_u64("999"), Ok(("", 999)));
    assert_eq!(uint_u64("0"), Ok(("", 0)));
    assert_eq!(uint_u64("0x100"), Ok(("", 256)));
    assert_eq!(uint_u64("0b101"), Ok(("", 5)));
    // We're not supposed to parse leading zeros.
    assert_eq!(uint_u64("00"), Ok(("0", 0)));
}

// Like RawUint, this is a temporary representation so we can
// preserve both the string slice and some metadata about it.
struct RawInt<'a> {
    slice: &'a str,
    base: u32,
    neg: bool,
}

// A signed integer
#[rustfmt::skip]
fn int(input: &str) -> JResult<&str, RawInt> {
    let f = pair(
        opt(charx('-')),
        uint
    );
    map(f, |(optneg, rawuint)| {
        RawInt {
            slice: rawuint.slice,
            base: rawuint.base,
            neg: optneg.is_some(),
        }
    })
    (input)
}

// "." fraction
// fraction = 1*DIGIT
#[rustfmt::skip]
fn dot_fraction(input: &str) -> JResult<&str, &str> {
    recognize(
        pair(
            charx('.'),
            digit1
        )
    )
    (input)
}

// "e" exponent
// exponent = ["+"/"-"] 1*DIGIT
#[rustfmt::skip]
fn e_exponent(input: &str) -> JResult<&str, &str> {
    recognize(
        tuple((
            charx('e'),
            opt(one_of("+-")),
            digit1
        ))
    )
    (input)
}

// A helper function for converting string -> Value::Float,
// and mapping to the right error type
#[rustfmt::skip]
fn parse_float(s: &str) -> Result<Value, ParseError> {
    match s.parse::<f64>() {
        Ok(fl) => Ok(Value::Float(fl)),
        Err(_) => Err(parse_error(MalformedFloat, s)),
    }
}

// A helper function for converting RawInt -> Value::Xint,
// and mapping to the right error type
fn parse_int(raw: RawInt) -> Result<Value, ParseError> {
    // Note: the string slice doesn't contain the '-' character, so we
    // need to handle that ourselves.
    let posint = u64::from_str_radix(raw.slice, raw.base)
        .map_err(|_| parse_error(MalformedInteger, raw.slice))?;

    if raw.neg {
        // i64 has a larger positive range than negative range, so if we
        // survive the conversion to i64 then unary `-` must succeed.
        let negint: i64 = try_into_int(posint, raw.slice)?;
        Ok(Value::Nint(-negint))
    } else {
        Ok(Value::Uint(posint))
    }
}

// A rough approximation of nom's offset() function, allowing us to
// do something like nom's recognize() without losing the inner types.
fn offset(whole: &str, part: &str) -> usize {
    part.as_ptr() as usize - whole.as_ptr() as usize
}

// This is similar to nom's `recognize` function.
// The difference is that it doesn't throw away the inner parser's result;
// it returns a tuple (slice, result) so you can have both.
fn recognizer<'a, O, E, F>(parser: F) -> impl Fn(&'a str) -> nom::IResult<&'a str, (&'a str, O), E>
where
    E: nom::error::ParseError<&'a str>,
    F: Fn(&'a str) -> nom::IResult<&'a str, O, E>,
{
    move |input: &'a str| {
        #[allow(clippy::clone_double_ref)]
        let i = input.clone();
        match parser(i) {
            Ok((i, output)) => {
                let index = offset(input, &i);
                let output_slice = &input[..index];
                let output_tuple = (output_slice, output);
                Ok((i, output_tuple))
            }
            Err(e) => Err(e),
        }
    }
}

// int ["." fraction] ["e" exponent ]
// (must have at least one of the latter two to be a float)
fn float_or_int(input: &str) -> JResult<&str, Value> {
    let f = recognizer(tuple((int, opt(dot_fraction), opt(e_exponent))));
    map_res_fail(f, |(recognized, output)| {
        let (firstint, frac, exp) = output;

        // If we picked up a '.' or 'e' we are parsing a float; if neither we
        // are parsing an integer.
        let dot_or_e = frac.is_some() || exp.is_some();
        if dot_or_e {
            parse_float(recognized)
        } else {
            parse_int(firstint)
        }
    })(input)
}

#[test]
fn test_float_or_int() {
    assert_eq!(float_or_int("0.0"), Ok(("", Value::Float(0.0))));
    assert_eq!(float_or_int("1e99"), Ok(("", Value::Float(1e99))));
    assert_eq!(float_or_int("-1e-99"), Ok(("", Value::Float(-1e-99))));
    assert_eq!(float_or_int("123"), Ok(("", Value::Uint(123))));
    assert_eq!(float_or_int("-123"), Ok(("", Value::Nint(-123))));
    assert_eq!(float_or_int("1e"), Ok(("e", Value::Uint(1))));
    assert_eq!(float_or_int("1."), Ok((".", Value::Uint(1))));
    assert!(float_or_int("abc").is_err());

    assert_eq!(float_or_int("0x100"), Ok(("", Value::Uint(256))));
    assert_eq!(float_or_int("0b101"), Ok(("", Value::Uint(5))));
    // We're not supposed to parse leading zeros.
    assert_eq!(float_or_int("00"), Ok(("0", Value::Uint(0))));

    assert_eq!(float_or_int("-0x100"), Ok(("", Value::Nint(-256))));
    assert_eq!(float_or_int("-0b101"), Ok(("", Value::Nint(-5))));

    // While this is allowed in the CDDL grammar, it doesn't make logical sense
    // so we want to return an error.
    assert!(float_or_int("0b1e99").is_err());
    assert!(float_or_int("0b1.1").is_err());
}

// bytes = [bsqual] %x27 *BCHAR %x27
// BCHAR = %x20-26 / %x28-5B / %x5D-10FFFD / SESC / CRLF
// bsqual = "h" / "b64"
//
// Byte strings can come in 3 forms:
// - 'abc\n' <= utf8 string interpreted as a byte string (and escaping is allowed)
// - h'1234' or h'12 34' <= hex (base16) bytes; all whitespace is ignored.
// - 'SGVsbG8gd29ybGQ=' <= base64 encoded byte string.
// Also, byte strings can be concatenated, i.e. 'Hello ' 'world' == 'Hello world'.
// See the RFC for details.

#[rustfmt::skip]
fn bytestring_utf8(input: &str) -> JResult<&str, &str> {
    delimited(
        charx('\''),
        alpha0, // FIXME: replace with BCHAR
        charx('\'')
    )(input)
}

#[rustfmt::skip]
fn bytestring_hex(input: &str) -> JResult<&str, &str> {
    delimited(
        tag("h'"),
        recognize(
            pair(
                ws,
                many0(terminated(hex_digit1, ws)),
            )
        ),
        charx('\'')
    )(input)
}

#[rustfmt::skip]
fn is_base64_char(c: char) -> bool {
    let ranges = [
        (0x30 ..= 0x39), // 0-9
        (0x41 ..= 0x5A), // A-Z
        (0x61 ..= 0x7A), // a-z
    ];
    let cv = c as u32;

    ranges.iter().any(|range| range.contains(&cv))
    || c == '+' || c == '/' || c == '='
}

// Zero or more base64 characters
#[rustfmt::skip]
fn base64(input: &str) -> JResult<&str, &str> {
    take_while(is_base64_char)
    (input)
}

#[rustfmt::skip]
fn bytestring_base64(input: &str) -> JResult<&str, &str> {
    delimited(
        tag("b64'"),
        base64,
        charx('\'')
    )(input)
}

// A helper function for parsing hex digits to bytes, while
// ignoring whitespace and mapping to the right error type.
fn parse_hex(s: &str) -> Result<Vec<u8>, ParseError> {
    // strip whitespace
    // FIXME: this consumes more chars than the RFC says we should.
    let s: String = s.chars().filter(|c| !c.is_ascii_whitespace()).collect();

    hex::decode(&s).map_err(|_| parse_error(MalformedHex, s))
}

#[rustfmt::skip]
fn bytestring(input: &str) -> JResult<&str, Vec<u8>> {
    alt((
        map(bytestring_utf8, |s| s.as_bytes().into()),
        map_res_fail(bytestring_hex, |s| parse_hex(s)),
        map(bytestring_base64, |s| s.as_bytes().into()), // FIXME: base64 decode here!
    ))
    (input)
}

#[test]
fn test_bytestring() {
    let result1 = bytestring("'abc'");
    let result = format!("{:?}", result1);
    assert_eq!(result, r#"Ok(("", [97, 98, 99]))"#);

    // Same thing, in hex format
    assert_eq!(result1, bytestring("h'61 62 63'"));

    // Same thing, in base64 format
    //assert_eq!(result1, bytestring("b64'YWJj'"));

    // FIXME: test invalid strings
}

// text = %x22 *SCHAR %x22
// SCHAR = %x20-21 / %x23-5B / %x5D-7E / %x80-10FFFD / SESC
// SESC = "\" (%x20-7E / %x80-10FFFD)

#[rustfmt::skip]
fn is_unescaped_schar(c: char) -> bool {
    let ranges = [
        (0x20 ..= 0x21),
        (0x23 ..= 0x58),
        (0x5D ..= 0x7E),
        (0x80 ..= 0x10FFD),
    ];
    let cv = c as u32;

    ranges.iter().any(|range| range.contains(&cv))
}

// One or more unescaped text characters
#[rustfmt::skip]
fn unescaped_schar(input: &str) -> JResult<&str, &str> {
    take_while1(is_unescaped_schar)
    (input)
}

// A single escaped character
#[rustfmt::skip]
fn sesc(input: &str) -> JResult<&str, &str> {
    // FIXME: allow only (%x20-7E / %x80-10FFFD)
    preceded(charx('\\'), recognize(anychar))
    (input)
}

// Zero or more text characters
#[rustfmt::skip]
fn schar(input: &str) -> JResult<&str, &str> {
    recognize(
        many0(
            alt((
                unescaped_schar,
                sesc
            ))
        )
    )
    (input)
}

#[rustfmt::skip]
fn text_literal(input: &str) -> JResult<&str, String> {
    let f = delimited(
        charx('"'),
        schar,
        charx('"')
    );

    map_res_fail(f, |s| {
        unescape(s).map_err(|_| parse_error(MalformedText, s) )
    })
    (input)
}

#[test]
fn test_text() {
    assert!(is_unescaped_schar('A'));
    assert!(is_unescaped_schar('の'));
    assert!(is_unescaped_schar(std::char::from_u32(0x10FF0).unwrap()));
    assert!(!is_unescaped_schar(0x7F as char));

    assert_eq!(unescaped_schar("Aの"), Ok(("", "Aの")));

    assert_eq!(sesc(r#"\n"#), Ok(("", "n")));
    assert_eq!(sesc(r#"\nn"#), Ok(("n", "n")));
    assert_eq!(sesc(r#"\の"#), Ok(("", "の")));

    // FIXME: sesc is allowing characters it shouldn't.
    // assert_eq!(sesc("\\\x7F"), Ok(("\\\x7F", "")));

    assert_eq!(schar(r#"Ab! \c の \\"#), Ok(("", r#"Ab! \c の \\"#)));
    assert_eq!(schar(r#"a\nb"#), Ok(("", r#"a\nb"#)));
    assert_eq!(schar("a\nb"), Ok(("\nb", "a")));

    assert!(text_literal("\"a\nb").is_err());
    assert!(text_literal("abc").is_err());

    assert_eq!(text_literal(r#""a\nb""#), Ok(("", "a\nb".into())));
    assert_eq!(text_literal(r#""\uD834\uDD1E""#), Ok(("", "𝄞".into())));
    assert_eq!(text_literal(r#""の""#), Ok(("", "の".into())));
}

// value = number / text / bytes
// number = hexfloat / (int ["." fraction] ["e" exponent ])
#[rustfmt::skip]
fn value(input: &str) -> JResult<&str, Value> {
    alt((
        float_or_int,
        map(text_literal, Value::Text),
        map(bytestring, Value::Bytes),
    ))(input)
}

#[test]
fn test_value() {
    assert_eq!(value("123"), Ok(("", Value::Uint(123))));
    assert_eq!(value(r#""abc""#), Ok(("", Value::Text("abc".into()))));
    assert!(value("abc").is_err());
}

// memberkey = type1 S ["^" S] "=>"
//           / bareword S ":"
//           / value S ":"

#[rustfmt::skip]
fn memberkey_type1(input: &str) -> JResult<&str, MemberKey> {
    let f = tuple((
        terminated(type1, ws),
        opt(terminated(tag("^"), ws)),
        tag("=>")
    ));
    map(f, |(ty1, cut, _)| MemberKey{
        val: MemberKeyVal::Type1(ty1),
        cut: cut.is_some(),
    })
    (input)
}

#[test]
fn test_memberkey_type1() {
    let result = memberkey_type1("a => b");
    let result = format!("{:?}", result);
    assert_eq!(
        result,
        r#"Ok((" b", MemberKey { val: Type1(Simple(Typename("a"))), cut: false }))"#
    );

    let result = memberkey_type1("a ^ => b");
    let result = format!("{:?}", result);
    assert_eq!(
        result,
        r#"Ok((" b", MemberKey { val: Type1(Simple(Typename("a"))), cut: true }))"#
    );
}

#[rustfmt::skip]
fn memberkey_bareword(input: &str) -> JResult<&str, MemberKey> {
    let f = separated_pair(
        ident,
        ws,
        tag(":")
    );
    map(f, |(id, _)| MemberKey{
        val: MemberKeyVal::Bareword(id.into()),
        cut: true,
    })
    (input)
}

#[test]
fn test_memberkey_bareword() {
    let result = memberkey_bareword("a:b");
    let result = format!("{:?}", result);
    assert_eq!(
        result,
        r#"Ok(("b", MemberKey { val: Bareword("a"), cut: true }))"#
    );
}

#[rustfmt::skip]
fn memberkey_value(input: &str) -> JResult<&str, MemberKey> {
    let f = separated_pair(
        value,
        ws,
        tag(":")
    );
    map(f, |(val, _)| MemberKey{
        val: MemberKeyVal::Value(val),
        cut: true,
    })
    (input)
}

#[rustfmt::skip]
fn memberkey(input: &str) -> JResult<&str, MemberKey> {
    alt((
        memberkey_type1,
        memberkey_bareword,
        memberkey_value
    ))
    (input)
}

// [memberkey S] type
#[rustfmt::skip]
fn grpent_member(input: &str) -> JResult<&str, Member> {
    let f = pair(
        terminated(opt(memberkey), ws),
        ty
    );
    map(f, |(key, value)| Member{ key, value } )
    (input)
}

#[test]
fn test_member() {
    let result = grpent_member("a:b");
    let result = format!("{:?}", result);
    assert_eq!(
        result,
        r#"Ok(("", Member { key: Some(MemberKey { val: Bareword("a"), cut: true }), value: Type([Simple(Typename("b"))]) }))"#
    );

    let result = grpent_member("foo");
    let result = format!("{:?}", result);
    assert_eq!(
        result,
        r#"Ok(("", Member { key: None, value: Type([Simple(Typename("foo"))]) }))"#
    );
}

#[rustfmt::skip]
fn grpent_parens(input: &str) -> JResult<&str, Group> {
    delimited(
        charx('('),
        delimited(
            ws,
            group,
            ws,
        ),
        charx(')')
    )(input)
}

#[test]
fn test_grpent_parens() {
    let result = grpent_parens("()");
    let result = format!("{:?}", result);
    assert_eq!(result, r#"Ok(("", Group([GrpChoice([])])))"#);
}

#[rustfmt::skip]
fn grpent_val(input: &str) -> JResult<&str, GrpEntVal> {
    alt((
        map(grpent_member, GrpEntVal::Member),
        map(ident, |s| GrpEntVal::Groupname(s.into())),
        map(grpent_parens, GrpEntVal::Parenthesized),
    ))
    (input)
}

#[test]
fn test_grpent_val() {
    let result = grpent_val("foo");
    let result = format!("{:?}", result);
    assert_eq!(
        result,
        r#"Ok(("", Member(Member { key: None, value: Type([Simple(Typename("foo"))]) })))"#
    );

    let result = grpent_val("17");
    let result = format!("{:?}", result);
    assert_eq!(
        result,
        r#"Ok(("", Member(Member { key: None, value: Type([Simple(Value(Uint(17)))]) })))"#
    );
}

// A helper function that does u64->usize conversion, returning
// ParseError(MalformedInteger) on failure.
fn try_into_int<T, U>(x: T, source: &str) -> Result<U, ParseError>
where
    U: TryFrom<T>,
{
    <U>::try_from(x).map_err(|_| parse_error(MalformedInteger, source))
}

// occur = [uint] "*" [uint]
//       / "+"
//       / "?"
#[rustfmt::skip]
fn occur_star(input: &str) -> JResult<&str, Occur> {
    let f = tuple((
        opt(uint_u64),
        tag("*"),
        opt(uint_u64),
    ));
    // FIXME: it's really not the parser's business to be inventing an upper
    // limit here.  Plus, the use of usize::MAX is kind of gross.
    // The parser should leave these as Option and leave it to others to
    // decide what to do with that.
    map_res(f, |tup| -> Result<Occur, ParseError> {
        if tup.0.is_none() && tup.2.is_none() {
            Ok(Occur::ZeroOrMore)
        } else {
            let lower: usize = match tup.0 {
                Some(n) => try_into_int(n, input)?,
                None => 0,
            };
            let upper: usize = match tup.2 {
                Some(n) => try_into_int(n, input)?,
                None => std::usize::MAX,
            };
            Ok(Occur::Numbered(lower, upper))
        }
    })
    (input)
}

#[rustfmt::skip]
fn occur(input: &str) -> JResult<&str, Occur> {
    alt((
        occur_star,
        valuex(Occur::OneOrMore, tag("+")),
        valuex(Occur::Optional, tag("?"))
    ))
    (input)
}

#[test]
fn test_occur() {
    assert_eq!(occur("?"), Ok(("", Occur::Optional)));
    assert_eq!(occur("+"), Ok(("", Occur::OneOrMore)));
    assert_eq!(occur("*"), Ok(("", Occur::ZeroOrMore)));
    assert_eq!(occur("*9"), Ok(("", Occur::Numbered(0, 9))));
    assert_eq!(occur("7*"), Ok(("", Occur::Numbered(7, std::usize::MAX))));
    assert_eq!(occur("7*9"), Ok(("", Occur::Numbered(7, 9))));
    assert_eq!(occur("0b100*0x10"), Ok(("", Occur::Numbered(4, 16))));
}

// grpent = [occur S] [memberkey S] type
//        / [occur S] groupname [genericarg]  ; preempted by above
//        / [occur S] "(" S group S ")"

#[rustfmt::skip]
fn grpent(input: &str) -> JResult<&str, GrpEnt> {
    let f = pair(
        opt(terminated(occur, ws)),
        grpent_val
    );
    map(f, |(occur, val)| GrpEnt{ occur, val } )
    (input)
}

#[test]
fn test_grpent() {
    let result = grpent("foo");
    let result = format!("{:?}", result);
    assert_eq!(
        result,
        r#"Ok(("", GrpEnt { occur: None, val: Member(Member { key: None, value: Type([Simple(Typename("foo"))]) }) }))"#
    );

    let result = grpent("foo: bar");
    let result = format!("{:?}", result);
    assert_eq!(
        result,
        r#"Ok(("", GrpEnt { occur: None, val: Member(Member { key: Some(MemberKey { val: Bareword("foo"), cut: true }), value: Type([Simple(Typename("bar"))]) }) }))"#
    );
}

// grpchoice = zero-or-more "grpent optional-comma"
#[rustfmt::skip]
fn grpchoice(input: &str) -> JResult<&str, GrpChoice> {
    let f = many0(
        terminated(grpent, optcom)
    );
    map(f, GrpChoice)
    (input)
}

#[test]
fn test_grpchoice_empty() {
    let result = grpchoice("");
    let result = format!("{:?}", result);
    assert_eq!(result, r#"Ok(("", GrpChoice([])))"#);
}

// group = grpchoice *(S "//" S grpchoice)
#[rustfmt::skip]
fn group(input: &str) -> JResult<&str, Group> {

    // It would have been great to write this as
    //  separated_nonempty_list(
    //      tag("//"),
    //      grpchoice)
    // but separated_nonempty_list returns an error if the
    // list-item succeeds on "", which grpchoice does.

    let f = pair(
        grpchoice,
        many0(preceded(
            delimited(
                ws,
                tag("//"),
                ws,
            ),
            grpchoice
        ))
    );

    map(f, |(first, mut rest)| {
        // Build a new vector containing all the grpchoice elements.
        let mut gcs = vec![first];
        gcs.append(&mut rest);
        Group(gcs)
    })(input)
}

#[test]
fn test_group_empty() {
    let result = group("");
    let result = format!("{:?}", result);
    assert_eq!(result, r#"Ok(("", Group([GrpChoice([])])))"#);
}

// "(" S type S ")"
#[rustfmt::skip]
fn type2_parens(input: &str) -> JResult<&str, Type> {
    delimited(
        charx('('),
        delimited(
            ws,
            ty,
            ws,
        ),
        charx(')')
    )(input)
}

// "{" S group S "}"
#[rustfmt::skip]
fn type2_map(input: &str) -> JResult<&str, Group> {
    delimited(
        charx('{'),
        delimited(
            ws,
            group,
            ws,
        ),
        charx('}')
    )(input)
}

// "[" S group S "]"
#[rustfmt::skip]
fn type2_array(input: &str) -> JResult<&str, Group> {
    delimited(
        charx('['),
        delimited(
            ws,
            group,
            ws,
        ),
        charx(']')
    )(input)
}

// "~" S typename [genericarg]
#[rustfmt::skip]
fn type2_unwrap(input: &str) -> JResult<&str, &str> {
    preceded(
        tag("~"),
        preceded(
            ws,
            ident
        )
    )
    (input)
}

// type2 = value
//       / typename [genericarg]
//       / "(" S type S ")"
//       / "{" S group S "}"
//       / "[" S group S "]"
//       / "~" S typename [genericarg]
//       / "&" S "(" S group S ")"
//       / "&" S groupname [genericarg]
//       / "#" "6" ["." uint] "(" S type S ")"
//       / "#" DIGIT ["." uint]
//       / "#"
#[rustfmt::skip]
fn type2(input: &str) -> JResult<&str, Type2> {
    alt((
        map(value, Type2::Value),
        map(ident, |i| Type2::Typename(i.into())),
        map(type2_parens, Type2::Parethesized),
        map(type2_map, Type2::Map),
        map(type2_array, Type2::Array),
        map(type2_unwrap, |s| Type2::Unwrap(s.into())),
    ))
    (input)
}

// Returns the string containing the control identifier
#[rustfmt::skip]
fn control_op(input: &str) -> JResult<&str, &str> {
    preceded(
        tag("."),
        ident
    )
    (input)
}

// Returns the range or control operator string
// (either ".." or "..." or the control identifier)
#[rustfmt::skip]
fn range_or_control_op(input: &str) -> JResult<&str, (&str, Type2)> {
    pair(
        alt((
            tag("..."),
            tag(".."),
            control_op
        )),
        preceded(
            ws,
            type2
        )
    )
    (input)
}

// type1 = type2 [S (rangeop / ctlop) S type2]
#[rustfmt::skip]
fn type1(input: &str) -> JResult<&str, Type1> {
    let f = pair(
        type2,
        opt(
            preceded(
                ws,
                range_or_control_op
            )
        )
    );
    map(f, |ty1| match ty1 {
        (ty2, None) => Type1::Simple(ty2),
        (start, Some(("..", end))) => Type1::Range(TypeRange {
            start,
            inclusive: true,
            end,
        }),
        (start, Some(("...", end))) => Type1::Range(TypeRange {
            start,
            inclusive: false,
            end,
        }),
        (first, Some((op, second))) => Type1::Control(TypeControl {
            first,
            op: op.into(),
            second,
        }),
    })
    (input)
}

#[test]
fn test_type1() {
    let result = type1("1 .. 9");
    let result = format!("{:?}", result);
    assert_eq!(
        result,
        r#"Ok(("", Range(TypeRange { start: Value(Uint(1)), end: Value(Uint(9)), inclusive: true })))"#
    );

    let result = type1("0x10 .. 0x1C");
    let result = format!("{:?}", result);
    assert_eq!(
        result,
        r#"Ok(("", Range(TypeRange { start: Value(Uint(16)), end: Value(Uint(28)), inclusive: true })))"#
    );

    let result = type1("1 ... 9");
    let result = format!("{:?}", result);
    assert_eq!(
        result,
        r#"Ok(("", Range(TypeRange { start: Value(Uint(1)), end: Value(Uint(9)), inclusive: false })))"#
    );

    let result = type1("uint .size 3");
    let result = format!("{:?}", result);
    assert_eq!(
        result,
        r#"Ok(("", Control(TypeControl { first: Typename("uint"), second: Value(Uint(3)), op: "size" })))"#
    );

    // RFC8610 2.2.2.1 points out that "min..max" is not a range, but an identifier
    // (because '.' is a valid ident character).
    let result = type2("min..max");
    let result = format!("{:?}", result);
    assert_eq!(result, r#"Ok(("", Typename("min..max")))"#);
}

// type = type1 [ / type1 ... ]  (skipping over type1 for now)
#[rustfmt::skip]
fn ty(input: &str) -> JResult<&str, Type> {
    let f = separated_nonempty_list(
        delimited(ws, tag("/"), ws),
        type1
    );
    map(f, Type)
    (input)
}

// rule = typename [genericparm] S assignt S type
//      / groupname [genericparm] S assigng S grpent
// Note that the first one ends with "type", while
// the second one ends with "group".
// So "foo = (bar)" will be forced down the second path.
//
// The most efficient parsing would be
// 1. name [genericparm] ws
// 2. = type
//    = grpent
//    /= type
//    //= grpent
//

// This is the right side of a rule: one of:
//     assignt S type
//     assigng S grpent
#[rustfmt::skip]
fn rule_val(input: &str) -> JResult<&str, RuleVal> {
    let f = separated_pair(
        tag("="),
        ws,
        alt((
            map(ty, RuleVal::AssignType),
            map(grpent, RuleVal::AssignGroup)
        ))
    );
    // We're just throwing away the operator for now, but we'll need it
    // later when we implement extend operators /= //=
    map(f, |(_op, val)| val )
    (input)
}

#[rustfmt::skip]
fn rule(input: &str) -> JResult<&str, Rule> {
    let f = separated_pair(
        ident,
        ws,
        rule_val
    );
    map(f, |(name, val)| Rule{ name: name.into(), val } )
    (input)
}

// cddl = S 1*(rule S)
#[rustfmt::skip]
fn cddl(input: &str) -> JResult<&str, Cddl> {
    let f = preceded(ws,
        many1(
            terminated(rule, ws)
        )
    );
    map(f, |r| Cddl{rules: r})
    (input)
}

#[rustfmt::skip]
fn cddl_slice(input: &str) -> JResult<&str, CddlSlice> {
    let f = preceded(ws,
        many1(
            terminated(
                map(recognizer(rule), |(s, r)| {
                    (r, s.to_string())
                }),
                ws
            )
        )
    );
    map(f, |r| CddlSlice{rules: r})
    (input)
}

/// The main entry point for parsing CDDL text.
///
/// If successful, it will return a [`Cddl`] instance containing all the rules
/// from the input text.
///
/// # Examples
/// ```
/// use cddl_cat::parse_cddl;
///
/// let input = "map = { name: tstr }";
/// assert!(parse_cddl(input).is_ok());
/// ```
///
pub fn parse_cddl(input: &str) -> Result<Cddl, ParseError> {
    let result = all_consuming(cddl)(input)?;
    Ok(result.1)
}

/// An entry point for parsing CDDL text, preserving rule strings
///
/// This operates exactly like [`parse_cddl`], but stores a copy of the rule's
/// original CDDL text.
pub fn slice_parse_cddl(input: &str) -> Result<CddlSlice, ParseError> {
    let result = all_consuming(cddl_slice)(input)?;
    Ok(result.1)
}

#[test]
fn test_grpchoice() {
    let result = grpchoice("abc");
    let result = format!("{:?}", result);
    assert_eq!(
        result,
        r#"Ok(("", GrpChoice([GrpEnt { occur: None, val: Member(Member { key: None, value: Type([Simple(Typename("abc"))]) }) }])))"#
    );

    let result = grpchoice("abc, def");
    let result = format!("{:?}", result);
    assert_eq!(
        result,
        r#"Ok(("", GrpChoice([GrpEnt { occur: None, val: Member(Member { key: None, value: Type([Simple(Typename("abc"))]) }) }, GrpEnt { occur: None, val: Member(Member { key: None, value: Type([Simple(Typename("def"))]) }) }])))"#
    );
}

#[test]
fn test_rule() {
    let result = rule("foo=bar").unwrap().1;
    let result = format!("{:?}", result);
    assert_eq!(
        result,
        r#"Rule { name: "foo", val: AssignType(Type([Simple(Typename("bar"))])) }"#
    );
}

#[test]
fn test_cddl() {
    let result = parse_cddl("foo = {\"a\": bar,\n b => baz}");
    let result = format!("{:?}", result);
    assert_eq!(
        result,
        r#"Ok(Cddl { rules: [Rule { name: "foo", val: AssignType(Type([Simple(Map(Group([GrpChoice([GrpEnt { occur: None, val: Member(Member { key: Some(MemberKey { val: Value(Text("a")), cut: true }), value: Type([Simple(Typename("bar"))]) }) }, GrpEnt { occur: None, val: Member(Member { key: Some(MemberKey { val: Type1(Simple(Typename("b"))), cut: false }), value: Type([Simple(Typename("baz"))]) }) }])])))])) }] })"#
    );
}

#[test]
fn test_cddl_slice() {
    let result = slice_parse_cddl(" foo = { a: tstr } bar = \n[ int ] ").unwrap();
    assert_eq!(result.rules[0].1, "foo = { a: tstr }");
    assert_eq!(result.rules[1].1, "bar = \n[ int ]");
}

// FIXME: these are things I discovered while validating cbor.  Move them to their own tests?
#[test]
fn test_stuff() {
    parse_cddl("thing = { foo : tstr }").unwrap();
    parse_cddl("bar = (c: int)").unwrap(); // This is a rule containing a group assignment.
    parse_cddl("thing = {agroup empty} agroup = (age: int, name: tstr) empty = ()").unwrap();
    parse_cddl(
        r#"
        address = { delivery }

        delivery = (
        street: tstr, ? "number": uint, city //
        po_box: uint, city //
        per_pickup: true )

        city = (
        name: tstr, zip_code: uint
        )"#,
    )
    .unwrap();
}

#[test]
fn test_errors() {
    let err = parse_cddl("x=9999999999999999999999999999999").unwrap_err();
    assert_eq!(err.kind, MalformedInteger);

    let err = parse_cddl(r#"x="\ud800""#).unwrap_err();
    assert_eq!(err.kind, MalformedText);

    let err = parse_cddl("x=h'61 62 6'").unwrap_err();
    assert_eq!(err.kind, MalformedHex);
}
