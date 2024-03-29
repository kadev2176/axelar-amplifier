use axelar_wasm_std::Participant;
use cosmwasm_std::{Addr, HexBinary, Uint256};

use crate::{
    key::{KeyType, PublicKey},
    worker_set::WorkerSet,
};

#[derive(Clone)]
pub struct TestSigner {
    pub address: Addr,
    pub pub_key: HexBinary,
    pub signature: HexBinary,
}

pub mod ecdsa_test_data {
    use super::*;

    pub fn pub_key() -> HexBinary {
        HexBinary::from_hex("03f57d1a813febaccbe6429603f9ec57969511b76cd680452dba91fa01f54e756d")
            .unwrap()
    }

    pub fn signature() -> HexBinary {
        HexBinary::from_hex("283786d844a7c4d1d424837074d0c8ec71becdcba4dd42b5307cb543a0e2c8b81c10ad541defd5ce84d2a608fc454827d0b65b4865c8192a2ea1736a5c4b7202")
            .unwrap()
    }

    pub fn message() -> HexBinary {
        HexBinary::from_hex("fa0609efd1dfeedfdcc8ba51520fae2d5176b7621d2560f071e801b0817e1537")
            .unwrap()
    }

    pub fn signers() -> Vec<TestSigner> {
        vec![
            TestSigner {
                address: Addr::unchecked("signer1"),
                pub_key: pub_key(),
                signature: signature(),
            },
            TestSigner {
                address: Addr::unchecked("signer2"),
                pub_key: pub_key(),
                signature: signature(),
            },
            TestSigner {
                address: Addr::unchecked("signer3"),
                pub_key: pub_key(),
                signature: signature(),
            },
        ]
    }
}

pub mod ed25519_test_data {
    use super::*;

    pub fn pub_key() -> HexBinary {
        HexBinary::from_hex("bc5b2bab5f08e332f85085388ff5d4c770ff82ecf7e5e8de0a4515318f7ef7e6")
            .unwrap()
    }

    pub fn signature() -> HexBinary {
        HexBinary::from_hex("e0876240536b548e5258b46126c6e0941e9da7c5ca3349d9e08f8cd4387ea919008766257c1eb72cc6c535ca678b8217076a23ac4e2ca4dee105aaf596bedd01")
            .unwrap()
    }

    pub fn message() -> HexBinary {
        HexBinary::from_hex("fa0609efd1dfeedfdcc8ba51520fae2d5176b7621d2560f071e801b0817e1537")
            .unwrap()
    }

    pub fn signers() -> Vec<TestSigner> {
        vec![
            TestSigner {
                address: Addr::unchecked("signer1"),
                pub_key: pub_key(),
                signature: signature(),
            },
            TestSigner {
                address: Addr::unchecked("signer2"),
                pub_key: pub_key(),
                signature: signature(),
            },
            TestSigner {
                address: Addr::unchecked("signer3"),
                pub_key: pub_key(),
                signature: signature(),
            },
        ]
    }
}

pub fn build_worker_set(key_type: KeyType, signers: &Vec<TestSigner>) -> WorkerSet {
    let mut total_weight = Uint256::zero();
    let participants = signers
        .iter()
        .map(|signer| {
            total_weight += Uint256::one();
            (
                Participant {
                    address: signer.address.clone(),
                    weight: Uint256::one().try_into().unwrap(),
                },
                PublicKey::try_from((key_type, signer.pub_key.clone())).unwrap(),
            )
        })
        .collect::<Vec<_>>();

    WorkerSet::new(participants, total_weight.mul_ceil((2u64, 3u64)), 0)
}
