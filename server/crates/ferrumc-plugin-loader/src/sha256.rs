use std::fs::File;
use std::io::{self, ErrorKind, Read};
use std::path::Path;

const BLOCK_BYTES: usize = 64;
const FILE_BUFFER_BYTES: usize = 64 * 1024;

const INITIAL_STATE: [u32; 8] = [
    0x6a09_e667,
    0xbb67_ae85,
    0x3c6e_f372,
    0xa54f_f53a,
    0x510e_527f,
    0x9b05_688c,
    0x1f83_d9ab,
    0x5be0_cd19,
];

const ROUND_CONSTANTS: [u32; 64] = [
    0x428a_2f98,
    0x7137_4491,
    0xb5c0_fbcf,
    0xe9b5_dba5,
    0x3956_c25b,
    0x59f1_11f1,
    0x923f_82a4,
    0xab1c_5ed5,
    0xd807_aa98,
    0x1283_5b01,
    0x2431_85be,
    0x550c_7dc3,
    0x72be_5d74,
    0x80de_b1fe,
    0x9bdc_06a7,
    0xc19b_f174,
    0xe49b_69c1,
    0xefbe_4786,
    0x0fc1_9dc6,
    0x240c_a1cc,
    0x2de9_2c6f,
    0x4a74_84aa,
    0x5cb0_a9dc,
    0x76f9_88da,
    0x983e_5152,
    0xa831_c66d,
    0xb003_27c8,
    0xbf59_7fc7,
    0xc6e0_0bf3,
    0xd5a7_9147,
    0x06ca_6351,
    0x1429_2967,
    0x27b7_0a85,
    0x2e1b_2138,
    0x4d2c_6dfc,
    0x5338_0d13,
    0x650a_7354,
    0x766a_0abb,
    0x81c2_c92e,
    0x9272_2c85,
    0xa2bf_e8a1,
    0xa81a_664b,
    0xc24b_8b70,
    0xc76c_51a3,
    0xd192_e819,
    0xd699_0624,
    0xf40e_3585,
    0x106a_a070,
    0x19a4_c116,
    0x1e37_6c08,
    0x2748_774c,
    0x34b0_bcb5,
    0x391c_0cb3,
    0x4ed8_aa4a,
    0x5b9c_ca4f,
    0x682e_6ff3,
    0x748f_82ee,
    0x78a5_636f,
    0x84c8_7814,
    0x8cc7_0208,
    0x90be_fffa,
    0xa450_6ceb,
    0xbef9_a3f7,
    0xc671_78f2,
];

struct Sha256 {
    state: [u32; 8],
    block: [u8; BLOCK_BYTES],
    block_len: usize,
    message_bits: u64,
}

impl Sha256 {
    const fn new() -> Self {
        Self {
            state: INITIAL_STATE,
            block: [0; BLOCK_BYTES],
            block_len: 0,
            message_bits: 0,
        }
    }

    fn update(&mut self, mut input: &[u8]) -> io::Result<()> {
        let added_bytes = u64::try_from(input.len()).map_err(|_| length_error())?;
        let added_bits = added_bytes.checked_mul(8).ok_or_else(length_error)?;
        self.message_bits = self
            .message_bits
            .checked_add(added_bits)
            .ok_or_else(length_error)?;

        while !input.is_empty() {
            let available = BLOCK_BYTES - self.block_len;
            let copied = available.min(input.len());
            let next_len = self.block_len + copied;
            self.block[self.block_len..next_len].copy_from_slice(&input[..copied]);
            self.block_len = next_len;
            input = &input[copied..];

            if self.block_len == BLOCK_BYTES {
                let block = self.block;
                self.compress(&block);
                self.block = [0; BLOCK_BYTES];
                self.block_len = 0;
            }
        }

        Ok(())
    }

    fn finalize(mut self) -> [u8; 32] {
        self.block[self.block_len] = 0x80;
        self.block_len += 1;

        if self.block_len > 56 {
            self.block[self.block_len..].fill(0);
            let block = self.block;
            self.compress(&block);
            self.block = [0; BLOCK_BYTES];
        } else {
            self.block[self.block_len..56].fill(0);
        }

        self.block[56..].copy_from_slice(&self.message_bits.to_be_bytes());
        let block = self.block;
        self.compress(&block);

        let mut digest = [0; 32];
        for (word, bytes) in self.state.iter().zip(digest.chunks_exact_mut(4)) {
            bytes.copy_from_slice(&word.to_be_bytes());
        }
        digest
    }

    fn compress(&mut self, block: &[u8; BLOCK_BYTES]) {
        let mut words = [0_u32; 64];
        for (index, word) in words.iter_mut().take(16).enumerate() {
            let offset = index * 4;
            *word = u32::from_be_bytes([
                block[offset],
                block[offset + 1],
                block[offset + 2],
                block[offset + 3],
            ]);
        }

        for index in 16..64 {
            let sigma_zero = words[index - 15].rotate_right(7)
                ^ words[index - 15].rotate_right(18)
                ^ (words[index - 15] >> 3);
            let sigma_one = words[index - 2].rotate_right(17)
                ^ words[index - 2].rotate_right(19)
                ^ (words[index - 2] >> 10);
            words[index] = words[index - 16]
                .wrapping_add(sigma_zero)
                .wrapping_add(words[index - 7])
                .wrapping_add(sigma_one);
        }

        let [mut working_a, mut working_b, mut working_c, mut working_d, mut working_e, mut working_f, mut working_g, mut working_h] =
            self.state;
        for (constant, word) in ROUND_CONSTANTS.iter().zip(words) {
            let choose = (working_e & working_f) ^ ((!working_e) & working_g);
            let majority =
                (working_a & working_b) ^ (working_a & working_c) ^ (working_b & working_c);
            let sum_zero =
                working_a.rotate_right(2) ^ working_a.rotate_right(13) ^ working_a.rotate_right(22);
            let sum_one =
                working_e.rotate_right(6) ^ working_e.rotate_right(11) ^ working_e.rotate_right(25);
            let temporary_one = working_h
                .wrapping_add(sum_one)
                .wrapping_add(choose)
                .wrapping_add(*constant)
                .wrapping_add(word);
            let temporary_two = sum_zero.wrapping_add(majority);

            working_h = working_g;
            working_g = working_f;
            working_f = working_e;
            working_e = working_d.wrapping_add(temporary_one);
            working_d = working_c;
            working_c = working_b;
            working_b = working_a;
            working_a = temporary_one.wrapping_add(temporary_two);
        }

        self.state[0] = self.state[0].wrapping_add(working_a);
        self.state[1] = self.state[1].wrapping_add(working_b);
        self.state[2] = self.state[2].wrapping_add(working_c);
        self.state[3] = self.state[3].wrapping_add(working_d);
        self.state[4] = self.state[4].wrapping_add(working_e);
        self.state[5] = self.state[5].wrapping_add(working_f);
        self.state[6] = self.state[6].wrapping_add(working_g);
        self.state[7] = self.state[7].wrapping_add(working_h);
    }
}

fn length_error() -> io::Error {
    io::Error::new(
        ErrorKind::InvalidData,
        "SHA-256 input length exceeds the 64-bit bit-length field",
    )
}

/// Computes the SHA-256 digest of a file with a fixed 64 KiB read buffer.
pub(crate) fn digest_file(path: &Path) -> io::Result<[u8; 32]> {
    let mut file = File::open(path)?;
    let mut buffer = vec![0; FILE_BUFFER_BYTES].into_boxed_slice();
    let mut hasher = Sha256::new();

    loop {
        let read = match file.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        hasher.update(&buffer[..read])?;
    }

    Ok(hasher.finalize())
}

#[cfg(test)]
mod tests {
    use std::fs::{self, File};
    use std::io::Write;
    use std::path::{Path, PathBuf};

    use super::{digest_file, Sha256, FILE_BUFFER_BYTES};

    const MILLION_A_DIGEST: &str =
        "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0";

    fn digest_bytes(input: &[u8]) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(input).expect("test input length is valid");
        hasher.finalize()
    }

    fn hex(digest: [u8; 32]) -> String {
        const DIGITS: &[u8; 16] = b"0123456789abcdef";

        let mut encoded = String::with_capacity(64);
        for byte in digest {
            encoded.push(char::from(DIGITS[usize::from(byte >> 4)]));
            encoded.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
        }
        encoded
    }

    fn scratch_file() -> PathBuf {
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
        let repository = manifest
            .parent()
            .and_then(Path::parent)
            .and_then(Path::parent)
            .expect("loader crate is nested under the repository's server workspace");
        repository
            .join(".codex-tmp")
            .join(format!("plugin-loader-sha256-{}.bin", std::process::id()))
    }

    #[test]
    fn standard_sha256_vectors_match() {
        let vectors = [
            (
                b"".as_slice(),
                "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            ),
            (
                b"abc".as_slice(),
                "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            ),
            (
                b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq".as_slice(),
                "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1",
            ),
        ];

        for (input, expected) in vectors {
            assert_eq!(hex(digest_bytes(input)), expected);
        }
    }

    #[test]
    fn padding_and_block_boundaries_match() {
        let vectors = [
            (
                55,
                "9f4390f8d30c2dd92ec9f095b65e2b9ae9b0a925a5258e241c9f1e910f734318",
            ),
            (
                56,
                "b35439a4ac6f0948b6d6f9e3c6af0f5f590ce20f1bde7090ef7970686ec6738a",
            ),
            (
                63,
                "7d3e74a05d7db15bce4ad9ec0658ea98e3f06eeecf16b4c6fff2da457ddc2f34",
            ),
            (
                64,
                "ffe054fe7ae0cb6dc65c3af9b61d5209f439851db43d0ba5997337df154668eb",
            ),
            (
                65,
                "635361c48bb9eab14198e76ea8ab7f1a41685d6ad62aa9146d301d4f17eb0ae0",
            ),
        ];

        for (length, expected) in vectors {
            assert_eq!(hex(digest_bytes(&vec![b'a'; length])), expected);
        }
    }

    #[test]
    fn incremental_chunking_matches_million_a_vector() {
        const CHUNK_SIZES: [usize; 10] = [1, 2, 3, 7, 63, 64, 65, 4095, 4096, 8193];

        let input = [b'a'; 8193];
        let mut remaining = 1_000_000;
        let mut next_chunk = 0;
        let mut hasher = Sha256::new();
        while remaining != 0 {
            let chunk = CHUNK_SIZES[next_chunk % CHUNK_SIZES.len()].min(remaining);
            hasher
                .update(&input[..chunk])
                .expect("million-byte standard vector fits SHA-256");
            remaining -= chunk;
            next_chunk += 1;
        }

        assert_eq!(hex(hasher.finalize()), MILLION_A_DIGEST);
    }

    #[test]
    fn digest_file_streams_a_real_file() {
        let path = scratch_file();
        let directory = path
            .parent()
            .expect("scratch file has a repository-local parent");
        fs::create_dir_all(directory).expect("repository scratch directory is writable");

        let mut file = File::create(&path).expect("real-file vector can be created");
        let buffer = vec![b'a'; FILE_BUFFER_BYTES];
        let full_buffers = 1_000_000 / FILE_BUFFER_BYTES;
        let remainder = 1_000_000 % FILE_BUFFER_BYTES;
        for _ in 0..full_buffers {
            file.write_all(&buffer)
                .expect("full vector buffer can be written");
        }
        file.write_all(&buffer[..remainder])
            .expect("final vector bytes can be written");
        drop(file);

        let digest = digest_file(&path).expect("real-file vector can be hashed");
        fs::remove_file(&path).expect("real-file vector can be removed");
        assert_eq!(hex(digest), MILLION_A_DIGEST);
    }
}
