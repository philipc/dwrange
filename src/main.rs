extern crate gimli;
extern crate memmap;
extern crate object;

use std::env;
use std::fs;
use std::mem;

fn main() {
    let path = env::args().nth(1).unwrap();
    let file = fs::File::open(path).unwrap();
    let map = unsafe { memmap::Mmap::map(&file).unwrap() };
    let object = &object::File::parse(&*map).unwrap();
    let endian = if object.is_little_endian() {
        gimli::RunTimeEndian::Little
    } else {
        gimli::RunTimeEndian::Big
    };

    fn load_section<'input, 'object, S, Endian>(
        object: &'object object::File<'input>,
        endian: Endian,
    ) -> S
    where
        S: gimli::Section<gimli::EndianBuf<'input, Endian>>,
        Endian: gimli::Endianity,
        'object: 'input,
    {
        let data = object.get_section(S::section_name()).unwrap_or(&[]);
        S::from(gimli::EndianBuf::new(data, endian))
    }

    let debug_abbrev: gimli::DebugAbbrev<_> = load_section(object, endian);
    let debug_info: gimli::DebugInfo<_> = load_section(object, endian);
    let debug_line: gimli::DebugLine<_> = load_section(object, endian);
    let debug_ranges: gimli::DebugRanges<_> = load_section(object, endian);
    let debug_str: gimli::DebugStr<_> = load_section(object, endian);

    let mut units = debug_info.units();
    while let Some(unit) = units.next().unwrap() {
        let abbrevs = unit.abbreviations(&debug_abbrev).unwrap();
        producer(&unit, &abbrevs, &debug_str).unwrap();

        let unit_ranges = unit_ranges(&unit, &abbrevs, &debug_ranges).unwrap();
        println!("unit:");
        unit_ranges.print();

        let line_ranges = line_ranges(&unit, &abbrevs, &debug_line, &debug_str).unwrap();
        println!("line:");
        line_ranges.print();

        let function_ranges = function_ranges(&unit, &abbrevs, &debug_ranges).unwrap();
        println!("function:");
        function_ranges.print();

        println!("unit vs line:");
        unit_ranges.cmp(&line_ranges);

        println!("unit vs function:");
        unit_ranges.cmp(&function_ranges);

        println!("line vs function:");
        line_ranges.cmp(&function_ranges);
    }
}

fn producer<R: gimli::Reader>(
    unit: &gimli::CompilationUnitHeader<R, R::Offset>,
    abbrevs: &gimli::Abbreviations,
    debug_str: &gimli::DebugStr<R>,
) -> Result<(), gimli::Error> {
    let mut entries = unit.entries(abbrevs);
    let (_, entry) = entries.next_dfs().unwrap().unwrap();
    if let Some(producer) = entry
        .attr(gimli::DW_AT_producer)?
        .and_then(|x| x.string_value(debug_str))
    {
        println!("\nproducer: {}", producer.to_string_lossy()?);
    } else {
        println!("\nproducer: <unknown>");
    }
    Ok(())
}

fn line_ranges<R: gimli::Reader>(
    unit: &gimli::CompilationUnitHeader<R, R::Offset>,
    abbrevs: &gimli::Abbreviations,
    debug_line: &gimli::DebugLine<R>,
    debug_str: &gimli::DebugStr<R>,
) -> Result<RangeList, gimli::Error> {
    let mut ranges = RangeList::default();
    let mut entries = unit.entries(abbrevs);
    let (_, entry) = entries.next_dfs().unwrap().unwrap();
    let comp_dir = entry
        .attr(gimli::DW_AT_comp_dir)?
        .and_then(|x| x.string_value(debug_str));
    let comp_name = entry
        .attr(gimli::DW_AT_name)?
        .and_then(|x| x.string_value(debug_str));
    let stmt_list = match entry.attr_value(gimli::DW_AT_stmt_list)? {
        Some(gimli::AttributeValue::DebugLineRef(val)) => val,
        _ => return Ok(ranges),
    };
    let program = debug_line.program(stmt_list, unit.address_size(), comp_dir, comp_name)?;
    let (_, sequences) = program.sequences()?;
    for sequence in sequences {
        if sequence.start != 0 {
            ranges.push(gimli::Range {
                begin: sequence.start,
                end: sequence.end,
            });
        }
    }
    ranges.sort();
    for w in ranges.ranges.windows(2) {
        if w[0].end > w[1].begin {
            println!("line overlap: {:?}", w);
        }
    }
    Ok(ranges)
}

fn unit_ranges<R: gimli::Reader>(
    unit: &gimli::CompilationUnitHeader<R, R::Offset>,
    abbrevs: &gimli::Abbreviations,
    debug_ranges: &gimli::DebugRanges<R>,
) -> Result<RangeList, gimli::Error> {
    let mut ranges = RangeList::default();
    let mut entries = unit.entries(abbrevs);
    let (_, entry) = entries.next_dfs().unwrap().unwrap();
    let base_addr = match entry.attr_value(gimli::DW_AT_low_pc)? {
        Some(gimli::AttributeValue::Addr(addr)) => addr,
        _ => 0,
    };
    match entry.attr_value(gimli::DW_AT_ranges)? {
        None => {
            let low_pc = match entry.attr_value(gimli::DW_AT_low_pc)? {
                Some(gimli::AttributeValue::Addr(low_pc)) => low_pc,
                _ => return Ok(ranges),
            };
            let high_pc = match entry.attr_value(gimli::DW_AT_high_pc)? {
                Some(gimli::AttributeValue::Addr(high_pc)) => high_pc,
                Some(gimli::AttributeValue::Udata(x)) => low_pc + x,
                _ => return Ok(ranges),
            };
            ranges.push(gimli::Range {
                begin: low_pc,
                end: high_pc,
            });
        }
        Some(gimli::AttributeValue::DebugRangesRef(rr)) => {
            let mut iter = debug_ranges.ranges(rr, unit.address_size(), base_addr)?;
            while let Some(range) = iter.next()? {
                ranges.push(range);
            }
            ranges.sort();
            for w in ranges.ranges.windows(2) {
                if w[0].end > w[1].begin {
                    println!("unit overlap: {:?}", w);
                }
            }
        }
        _ => {}
    }
    Ok(ranges)
}

fn function_ranges<R: gimli::Reader>(
    unit: &gimli::CompilationUnitHeader<R, R::Offset>,
    abbrevs: &gimli::Abbreviations,
    debug_ranges: &gimli::DebugRanges<R>,
) -> Result<RangeList, gimli::Error> {
    let mut ranges = RangeList::default();
    let mut entries = unit.entries(abbrevs);
    let base_addr;
    {
        let (_, entry) = entries.next_dfs()?.unwrap();
        base_addr = match entry.attr_value(gimli::DW_AT_low_pc)? {
            Some(gimli::AttributeValue::Addr(addr)) => addr,
            _ => 0,
        };
    }
    while let Some((_, entry)) = entries.next_dfs()? {
        let tag = entry.tag();
        if tag != gimli::DW_TAG_subprogram {
            continue;
        }
        match entry.attr_value(gimli::DW_AT_ranges)? {
            None => {
                let low_pc = match entry.attr_value(gimli::DW_AT_low_pc)? {
                    Some(gimli::AttributeValue::Addr(low_pc)) => low_pc,
                    _ => continue,
                };
                let high_pc = match entry.attr_value(gimli::DW_AT_high_pc)? {
                    Some(gimli::AttributeValue::Addr(high_pc)) => high_pc,
                    Some(gimli::AttributeValue::Udata(x)) => low_pc + x,
                    _ => continue,
                };
                if low_pc != 0 {
                    ranges.push(gimli::Range {
                        begin: low_pc,
                        end: high_pc,
                    });
                }
            }
            Some(gimli::AttributeValue::DebugRangesRef(rr)) => {
                let mut iter = debug_ranges.ranges(rr, unit.address_size(), base_addr)?;
                while let Some(range) = iter.next()? {
                    ranges.push(range);
                }
            }
            _ => {}
        }
    }
    ranges.sort();
    for w in ranges.ranges.windows(2) {
        if w[0].end > w[1].begin {
            println!("function overlap: {:?}", w);
        }
    }
    Ok(ranges)
}

#[derive(Debug, Default, Clone)]
pub(crate) struct RangeList {
    pub ranges: Vec<gimli::Range>,
}

impl RangeList {
    // Append a range, combining with previous range if possible.
    pub fn push(&mut self, range: gimli::Range) {
        if range.end < range.begin {
            eprintln!("invalid range: {:?}", range);
            return;
        }
        if range.end == range.begin {
            return;
        }
        if let Some(prev) = self.ranges.last_mut() {
            // Assume up to 15 bytes of padding if range.begin is aligned.
            // (This may be a wrong assumption, but does it matter and
            // how do we do better?)
            let padding = if range.begin == range.begin & !15 {
                15
            } else {
                0
            };
            // Merge ranges if new range begins at end of previous range.
            // We don't care about merging in opposite order (that'll happen
            // when sorting).
            if range.begin >= prev.end && range.begin <= prev.end + padding {
                if prev.end < range.end {
                    prev.end = range.end;
                }
                return;
            }
        }
        self.ranges.push(range);
    }

    pub fn sort(&mut self) {
        self.ranges.sort_by(|a, b| a.begin.cmp(&b.begin));
        // Combine ranges by adding to a new list.
        let mut ranges = Vec::new();
        mem::swap(&mut ranges, &mut self.ranges);
        for range in ranges {
            self.push(range);
        }
    }

    pub fn subtract(&self, other: &Self) -> Self {
        let mut ranges = RangeList::default();
        let mut other_ranges = other.ranges.iter();
        let mut other_range = other_ranges.next();
        for range in &*self.ranges {
            let mut range = *range;
            loop {
                match other_range {
                    Some(r) => {
                        // Is r completely before range?
                        if r.end <= range.begin {
                            other_range = other_ranges.next();
                            continue;
                        }
                        // Is r completely after range?
                        if r.begin >= range.end {
                            ranges.push(range);
                            break;
                        }
                        // Do we need to keep the head of the range?
                        if r.begin > range.begin {
                            ranges.push(gimli::Range {
                                begin: range.begin,
                                end: r.begin,
                            });
                        }
                        // Do we need to keep the tail of the range?
                        if r.end < range.end {
                            range.begin = r.end;
                            other_range = other_ranges.next();
                            continue;
                        }
                        break;
                    }
                    None => {
                        ranges.push(range);
                        break;
                    }
                }
            }
        }
        ranges.sort();
        ranges
    }

    pub fn print(&self) {
        for range in &self.ranges {
            println!("{:x}..{:x}", range.begin, range.end);
        }
    }

    pub fn cmp(&self, other: &Self) {
        let ranges = self.subtract(other);
        for range in ranges.ranges {
            println!("- {:x}..{:x}", range.begin, range.end);
        }
        let ranges = other.subtract(self);
        for range in ranges.ranges {
            println!("+ {:x}..{:x}", range.begin, range.end);
        }
    }
}
