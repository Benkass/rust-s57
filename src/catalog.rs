//! The catalog.rs provides the functionality to parse a CATALOG.031 file. It tries to use the
//! nomenclature from the S-57 data specification found in the Annex A section of
//! [`S-57 Specification`](http://iho.int/iho_pubs/standard/S-57Ed3.1/31Main.pdf). When reading it, remember to also keep
//! the maintenance document [`S-57 Maintenance`](http://iho.int/iho_pubs/maint/S57md8.pdf) close by since this section
//! in particular has alot of corrections.
use crate::data_parser::{Data, ParseData};
use crate::error::{Error, ErrorKind};
use failure::ResultExt;
use std::collections::HashMap;
use std::fmt::{Display, Formatter};
use std::io::{Read, Seek, SeekFrom};
use std::str::{from_utf8, FromStr};

const DRID: &'static str = "DRID";
const TOPLVL: &'static str = "0001";

pub(crate) const RECORD_SEPARATOR: u8 = 0x1e;
pub(crate) const UNIT_SEPARATOR: u8 = 0x1f;

#[derive(Debug, PartialEq)]
struct Leader {
    rl: usize,      // Record Length
    il: char,       // Interchange Level
    li: char,       // Leader Identifier
    cei: char,      // In Line Code Extension Indicator
    vn: char,       // Verison number
    ai: char,       // Application Indicator
    fcl: [char; 2], // Field Control Length
    ba: u32,        // Base Address Of Field Area
    csi: [char; 3], // Extended Character Set Indicator
    // Values of Entry Map
    flf: usize, // Size Of Field Length Field
    fpf: usize, // Size Of Field Position Field
    rsv: char,  // Reserved
    ftf: usize, // Size Of Field Tag Field
}

#[derive(Debug, PartialEq)]
pub(crate) struct DirectoryEntry {
    id: String,    // The Id of the field
    length: usize, // The length of the field in bytes
    offset: usize, // The offset in bytes form the start of the record
}

impl Display for DirectoryEntry {
    fn fmt(&self, f: &mut Formatter) -> std::fmt::Result {
        Display::fmt(&self.id, f)
    }
}

#[derive(Debug, PartialEq)]
enum DataStructureCode {
    SDI, // Single Data Item
    LS,  // Linear Structure
    MDS, // Multi-Dimensional structure
}

impl FromStr for DataStructureCode {
    type Err = crate::error::Error;
    fn from_str(value: &str) -> Result<DataStructureCode> {
        match value {
            "0" => Ok(DataStructureCode::SDI),
            "1" => Ok(DataStructureCode::LS),
            "2" => Ok(DataStructureCode::MDS),
            _ => Err(ErrorKind::BadDataStructureCode(value.to_string()).into()),
        }
    }
}

#[derive(Debug, PartialEq)]
enum DataTypeCode {
    CS,  // Character String
    IP,  // Implicit Point
    EP,  // Explicit Point (Real)
    BF,  // Binary Form
    MDT, // Mixed Data Types
}
impl FromStr for DataTypeCode {
    type Err = Error;
    fn from_str(value: &str) -> Result<DataTypeCode> {
        match value {
            "0" => Ok(DataTypeCode::CS),
            "1" => Ok(DataTypeCode::IP),
            "2" => Ok(DataTypeCode::EP),
            "5" => Ok(DataTypeCode::BF),
            "6" => Ok(DataTypeCode::MDT),
            _ => Err(ErrorKind::BadDataTypeCode(value.to_string()).into()),
        }
    }
}

// Truncated Escape Sequence
#[derive(Debug, PartialEq)]
enum TruncEscSeq {
    LE0, //Lexical Level 0
    LE1, //Lexical Level 1
    LE2, //Lexical Level 2
}
impl FromStr for TruncEscSeq {
    type Err = Error;
    fn from_str(value: &str) -> Result<TruncEscSeq> {
        match value {
            "   " => Ok(TruncEscSeq::LE0),
            "-A " => Ok(TruncEscSeq::LE1),
            "%/A" => Ok(TruncEscSeq::LE2),
            _ => Err(ErrorKind::BadTruncEscSeq(value.to_string()).into()),
        }
    }
}

#[derive(Debug, PartialEq)]
struct FileControlField {
    dsc: DataStructureCode,
    dtc: DataTypeCode,
}

#[derive(Debug, PartialEq)]
struct FieldControls {
    dsc: DataStructureCode,
    dtc: DataTypeCode,
    aux: String, // Auxilliary controls
    prt: String, // Printable graphics
    tes: TruncEscSeq,
}

// Data Descriptive Field Entry
#[derive(Debug, PartialEq)]
struct DDFEntry {
    fic: FieldControls,
    name: String,
    foc: Vec<(String, ParseData)>,
}

pub type Result<T> = std::result::Result<T, Error>;

pub(crate) fn parse_to_usize(bytes: &[u8]) -> Result<usize> {
    let s = from_utf8(bytes).with_context(|&err| ErrorKind::UtfError(err))?;
    Ok(s.parse().with_context(|err: &std::num::ParseIntError| {
        ErrorKind::ParseIntError(err.clone(), s.to_string())
    })?)
}

pub(crate) fn parse_to_string(bytes: &[u8]) -> Result<String> {
    Ok(from_utf8(bytes)
        .with_context(|&err| ErrorKind::UtfError(err))?
        .to_string())
}

fn parse_leader(byte: &[u8], len: usize) -> Result<Leader> {
    let rl = len;
    let il = byte[0] as char;
    let li = byte[1] as char;
    let cei = byte[2] as char;
    let vn = byte[3] as char;
    let ai = byte[4] as char;
    let fcl = [byte[5] as char, byte[6] as char];
    let ba = parse_to_usize(&byte[7..12]).context(ErrorKind::InvalidLeader)? as u32;
    let csi = [byte[12] as char, byte[13] as char, byte[14] as char];
    let flf = parse_to_usize(&byte[15..16]).context(ErrorKind::InvalidLeader)?;
    let fpf = parse_to_usize(&byte[16..17]).context(ErrorKind::InvalidLeader)?;
    let rsv = byte[17] as char;
    let ftf = parse_to_usize(&byte[18..19]).context(ErrorKind::InvalidLeader)?;
    Ok(Leader {
        rl,
        il,
        li,
        cei,
        vn,
        ai,
        fcl,
        ba,
        csi,
        flf,
        fpf,
        rsv,
        ftf,
    })
}

// TODO: Change this function to use exact_chunk when it is stable
fn parse_directory(byte: &[u8], leader: &Leader) -> Result<Vec<DirectoryEntry>> {
    let chunksize = leader.ftf + leader.flf + leader.fpf;
    let dir_iter = byte.chunks(chunksize);
    let mut directories: Vec<DirectoryEntry> = Vec::new();
    for d in dir_iter {
        if d.len() != chunksize {
            return Err(ErrorKind::BadDirectoryData.into());
        }
        let id = parse_to_string(&d[..leader.ftf])?;
        let length = parse_to_usize(&d[leader.ftf..leader.ftf + leader.flf])?;
        let offset = parse_to_usize(&d[leader.ftf + leader.flf..])?;

        directories.push(DirectoryEntry { id, length, offset });
    }

    Ok(directories)
}

fn parse_field_controls(byte: &[u8]) -> Result<FieldControls> {
    let dsc = from_utf8(&byte[0..1])
        .with_context(|&err| ErrorKind::UtfError(err))?
        .parse::<DataStructureCode>()
        .context(ErrorKind::BadFieldControl)?;
    let dtc = from_utf8(&byte[1..2])
        .with_context(|&err| ErrorKind::UtfError(err))?
        .parse::<DataTypeCode>()
        .context(ErrorKind::BadFieldControl)?;
    let aux = parse_to_string(&byte[2..4])?;
    let prt = parse_to_string(&byte[4..6])?;
    let tes = from_utf8(&byte[6..])
        .with_context(|&err| ErrorKind::UtfError(err))?
        .parse::<TruncEscSeq>()
        .context(ErrorKind::BadFieldControl)?;

    Ok(FieldControls {
        dsc,
        dtc,
        aux,
        prt,
        tes,
    })
}

fn parse_array_descriptors(byte: &[u8]) -> Result<Vec<String>> {
    if byte.is_empty() {
        // The Record Identifier is an unnamed descriptor and therefore the byte
        // array is empty. Since this is a key in a HashMap I use the name DRID
        // (Data Record ID) to identify this field.
        Ok(vec![String::from(DRID)])
    } else {
        Ok(parse_to_string(&byte[..])?
            .split('!')
            .map(String::from)
            .collect::<Vec<String>>())
    }
}

fn parse_format_controls(byte: &[u8]) -> Result<Vec<ParseData>> {
    if byte.len() < 2 {
        Err(ErrorKind::EmptyFormatControls.into())
    } else {
        // Remove surrounding parenthesies and create ParseDatas
        Ok(parse_to_string(&byte[1..byte.len() - 1])?
            .split(',')
            .map(|fc| ParseData::from_str(fc))
            .collect::<Result<Vec<(usize, ParseData)>>>()?
            .into_iter()
            .flat_map(|pd| std::iter::repeat(pd.1).take(pd.0))
            .collect())
    }
}

fn parse_ddfs(byte: &[u8], dirs: &[DirectoryEntry]) -> Result<HashMap<String, DDFEntry>> {
    // We should absolutely handle the file control field... later... but for now we skip it.
    dirs.iter()
        .skip(1)
        .map(|dir| {
            let s = dir.offset;
            //  take -1 to remove the record separator from the slice
            let e = dir.offset + dir.length - 1;
            let ddf_entry = parse_ddf(&byte[s..e]).context(ErrorKind::InvalidDDFS)?;
            Ok((dir.id.clone(), ddf_entry))
        })
        .collect()
}

fn parse_ddf(byte: &[u8]) -> Result<DDFEntry> {
    let parts = byte.split(|&b| b == UNIT_SEPARATOR).collect::<Vec<&[u8]>>();
    let (fic_bytes, name_bytes) = parts.get(0).ok_or(ErrorKind::InvalidHeader)?.split_at(9);
    let name = parse_to_string(name_bytes).context(ErrorKind::CouldNotParseName)?;
    let fic = parse_field_controls(fic_bytes).context(ErrorKind::InvalidDDF(name.clone()))?;
    let array_desc =
        parse_array_descriptors(parts.get(1).ok_or(ErrorKind::InvalidDDF(name.clone()))?)
            .context(ErrorKind::InvalidDDF(name.clone()))?;
    let data_parser =
        parse_format_controls(parts.get(2).ok_or(ErrorKind::InvalidDDF(name.clone()))?)
            .context(ErrorKind::InvalidDDF(name.clone()))?;
    if array_desc.len() == data_parser.len() {
        let foc = array_desc
            .into_iter()
            .zip(data_parser.into_iter())
            .collect();
        Ok(DDFEntry { fic, name, foc })
    } else {
        Err(ErrorKind::InvalidDDF(name.clone()).into())
    }
}

#[derive(Debug)]
struct DDR {
    dirs: Vec<DirectoryEntry>,
    // file_control_field,
    data_descriptive_fields: HashMap<String, DDFEntry>,
}

#[derive(Debug)]
pub struct Catalog<R: Read> {
    ddr: DDR, // Data Descriptive Record
    rdr: R,   // reader to ask for Data Records
}

#[derive(Debug)]
pub struct Record(HashMap<String, Field>);

pub type Field = HashMap<String, Data>;

impl Record {
    pub fn id(&self) -> Option<i64> {
        self.0.get(TOPLVL).and_then(|m| m.get(DRID)).and_then(|v| {
            if let Data::Integer(i) = v {
                *i
            } else {
                None
            }
        })
    }

    pub fn get(&self, arr_desc: &str) -> Option<&Field> {
        self.0.get(arr_desc)
    }
}

impl<R: Read> Catalog<R> {
    pub fn new(mut rdr: R) -> Result<Catalog<R>> {
        let ddr = parse_ddr(&mut rdr).context(ErrorKind::CouldNotParseCatalog)?;
        Ok(Catalog { ddr, rdr })
    }

    fn parse_dr(&mut self) -> Result<Option<Record>> {
        let (dirs, field_data) = match parse_dir_and_field_area(&mut self.rdr) {
            Ok(ok) => ok,
            Err(err) => match err.kind() {
                ErrorKind::EOF => return Ok(None),
                _ => return Err(err),
            },
        };
        let mut cur = std::io::Cursor::new(field_data);
        let mut record = Record(HashMap::new());
        for dir_entry in dirs.iter() {
            let ddf_entry = self
                .ddr
                .data_descriptive_fields
                .get(&dir_entry.id)
                .ok_or(ErrorKind::InvalidDR)?;
            let field_area = ddf_entry
                .foc
                .iter()
                .map(|(name, parser)| Ok((name.clone(), parser.parse(&mut cur)?)))
                .collect::<Result<Field>>()
                .context(ErrorKind::InvalidDR)?;
            // "Jump over" the last RECORD_SEPARATOR byte
            cur.seek(SeekFrom::Current(1))
                .with_context(|err| ErrorKind::IOError(err.kind()))?;
            record.0.insert(dir_entry.id.clone(), field_area);
        }
        Ok(Some(record))
    }
}

impl<R: Read> Iterator for Catalog<R> {
    type Item = Result<Record>;
    fn next(&mut self) -> Option<Self::Item> {
        match self.parse_dr() {
            Ok(Some(dr)) => Some(Ok(dr)),
            Ok(None) => None,
            Err(err) => Some(Err(err)),
        }
    }
}
fn parse_dir_and_field_area<R: Read>(rdr: &mut R) -> Result<(Vec<DirectoryEntry>, Vec<u8>)> {
    // Read the length of the DDR, stored in the first 5 bytes
    let mut len_bytes = [0; 5];
    let nr_of_bytes = rdr
        .read(&mut len_bytes)
        .with_context(|err| ErrorKind::IOError(err.kind()))?;
    match nr_of_bytes {
        0 => return Err(ErrorKind::EOF.into()),
        5 => (),
        _ => return Err(ErrorKind::IOError(std::io::ErrorKind::UnexpectedEof).into()),
    }

    // Read the rest of the DDR
    let length = parse_to_usize(&len_bytes)?;
    let mut data = vec![0; length - 5];
    rdr.read_exact(&mut data)
        .with_context(|err| ErrorKind::IOError(err.kind()))?;
    let leader = parse_leader(&data[..19], length)?;
    let field_area_idx = match data.iter().position(|&b| b == RECORD_SEPARATOR) {
        Some(index) => index,
        None => return Err(ErrorKind::BadDirectoryData.into()),
    };
    let dirs = parse_directory(&data[19..field_area_idx], &leader)?;
    Ok((dirs, data[field_area_idx + 1..].to_vec()))
}

fn parse_ddr<R: Read>(rdr: &mut R) -> Result<DDR> {
    let (dirs, field_area) = parse_dir_and_field_area(rdr)?;
    let data_descriptive_fields = parse_ddfs(&field_area, &dirs).context(ErrorKind::InvalidDDR)?;

    Ok(DDR {
        dirs,
        data_descriptive_fields,
    })
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::data_parser::ParseType;

    fn get_test_leader() -> Leader {
        Leader {
            rl: 241,
            il: '3',
            li: 'L',
            cei: 'E',
            vn: '1',
            ai: ' ',
            fcl: ['0', '9'],
            ba: 58,
            csi: [' ', '!', ' '],
            flf: 3,
            fpf: 4,
            rsv: '0',
            ftf: 4,
        }
    }

    fn get_test_directory() -> Vec<DirectoryEntry> {
        vec![
            DirectoryEntry {
                id: "0000".to_string(),
                length: 19,
                offset: 0,
            },
            DirectoryEntry {
                id: "0001".to_string(),
                length: 44,
                offset: 19,
            },
            DirectoryEntry {
                id: "CATD".to_string(),
                length: 120,
                offset: 63,
            },
        ]
    }

    fn get_test_field_controls() -> FieldControls {
        FieldControls {
            dsc: DataStructureCode::LS,
            dtc: DataTypeCode::MDT,
            aux: "00".to_string(),
            prt: ";&".to_string(),
            tes: TruncEscSeq::LE1,
        }
    }

    fn get_test_format_controls() -> Vec<ParseData> {
        vec![
            ParseData::Fixed(ParseType::String, 2),
            ParseData::Fixed(ParseType::Integer, 10),
            ParseData::Fixed(ParseType::Integer, 10),
            ParseData::Variable(ParseType::Float),
            ParseData::Variable(ParseType::Float),
        ]
    }

    #[test]
    fn test_parse_leader() {
        let length = 241;
        let leader = "3LE1 0900058 ! 3404".as_bytes();
        let expected = get_test_leader();
        let actual = parse_leader(leader, length).unwrap();
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_parse_directory() {
        let leader = get_test_leader();
        let directory = "0000019000000010440019CATD1200063".as_bytes();
        let expected = get_test_directory();
        let actual = parse_directory(directory, &leader).unwrap();
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_parse_field_controls() {
        let field_controls = "1600;&-A ".as_bytes();
        let expected = get_test_field_controls();
        let actual = parse_field_controls(field_controls).unwrap();
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_parse_array_descriptor() {
        let array_descriptor =
            "RCNM!RCID!FILE!LFIL!VOLM!IMPL!SLAT!WLON!NLAT!ELON!CRCS!COMT".as_bytes();
        let expected = vec![
            "RCNM", "RCID", "FILE", "LFIL", "VOLM", "IMPL", "SLAT", "WLON", "NLAT", "ELON", "CRCS",
            "COMT",
        ];
        let actual = parse_array_descriptors(array_descriptor).unwrap();
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_parse_format_controls_with_empty() {
        let array_descriptor = &[0u8; 0];
        assert!(parse_format_controls(array_descriptor).is_err())
    }

    #[test]
    fn test_parse_format_controls() {
        let format_controls = "(A(2),2I(10),2R)".as_bytes();
        let expected = get_test_format_controls();
        let actual = parse_format_controls(format_controls).unwrap();
        assert_eq!(actual, expected);
    }
}
