//! Parsing ELF64 headers and program headers of static executables (for `loader`).

pub const PT_LOAD: u32 = 1;
pub const PT_PHDR: u32 = 6;
pub const PF_X: u32 = 1;
pub const PF_W: u32 = 2;

pub struct Elf<'a> {
    data: &'a [u8],
    pub entry: u64,
    pub phoff: u64,
    pub phentsize: u16,
    pub phnum: u16,
}

pub struct ProgramHeader {
    pub kind: u32,
    pub flags: u32,
    pub offset: u64,
    pub vaddr: u64,
    pub filesz: u64,
    pub memsz: u64,
}

fn u16_at(d: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([d[o], d[o + 1]])
}

fn u32_at(d: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(d[o..o + 4].try_into().unwrap())
}

fn u64_at(d: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(d[o..o + 8].try_into().unwrap())
}

/// The ELF header alone: where the program header table ends.
pub struct Header {
    phoff: u64,
    phentsize: u16,
    phnum: u16,
}

impl Header {
    /// The end of the program header table in the file (None: overflow).
    pub fn table_end(&self) -> Option<u64> {
        self.phoff.checked_add(self.phentsize as u64 * self.phnum as u64)
    }
}

impl<'a> Elf<'a> {
    /// Checks the ELF header (the first 64 bytes).
    pub fn parse_header(data: &[u8]) -> Result<Header, &'static str> {
        let elf = Elf::check(data)?;
        Ok(Header { phoff: elf.phoff, phentsize: elf.phentsize, phnum: elf.phnum })
    }

    /// The ELF header and the program header table (`data` covers both).
    pub fn parse(data: &'a [u8]) -> Result<Self, &'static str> {
        let elf = Self::check(data)?;
        let table_end = elf.phoff.checked_add(elf.phentsize as u64 * elf.phnum as u64).ok_or("corrupt program header table")?;
        if elf.phentsize < 56 || table_end > data.len() as u64 {
            return Err("corrupt program header table");
        }
        Ok(elf)
    }

    fn check(data: &'a [u8]) -> Result<Self, &'static str> {
        if data.len() < 64 || &data[0..4] != b"\x7fELF" {
            return Err("not an ELF file");
        }
        if data[4] != 2 || data[5] != 1 {
            return Err("only 64-bit little-endian ELF");
        }
        if u16_at(data, 16) != 2 {
            return Err("only static executables (ET_EXEC)");
        }
        if u16_at(data, 18) != 0x3e {
            return Err("not x86_64");
        }
        Ok(Elf { data, entry: u64_at(data, 24), phoff: u64_at(data, 32), phentsize: u16_at(data, 54), phnum: u16_at(data, 56) })
    }

    pub fn program_headers(&self) -> impl Iterator<Item = ProgramHeader> + '_ {
        (0..self.phnum as usize).map(move |i| {
            let o = self.phoff as usize + i * self.phentsize as usize;
            let d = self.data;
            ProgramHeader {
                kind: u32_at(d, o),
                flags: u32_at(d, o + 4),
                offset: u64_at(d, o + 8),
                vaddr: u64_at(d, o + 16),
                filesz: u64_at(d, o + 32),
                memsz: u64_at(d, o + 40),
            }
        })
    }
}
