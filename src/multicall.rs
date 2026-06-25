use alloy::primitives::{Address, Bytes};
use alloy::providers::Provider;
use alloy::rpc::types::BlockId;
use alloy::sol;
use anyhow::{Result, bail};

sol! {
    #[sol(rpc)]
    #[derive(Debug)]
    interface IMulticall3 {
        struct Call3 {
            address target;
            bool allowFailure;
            bytes callData;
        }
        struct Result {
            bool success;
            bytes returnData;
        }
        function aggregate3(Call3[] calldata calls) external payable returns (Result[] memory returnData);
    }
}

/// Canonical CREATE2 Multicall3 address (same on every supported chain).
pub const CANONICAL_MULTICALL3: &str = "0xcA11bde05977b3631167028862bE2a173976CA11";

/// A single planned view-function call: opaque calldata to a target address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallRequest {
    pub target: Address,
    pub call_data: Bytes,
}

/// Per-call outcome; order matches the input `CallRequest` slice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallResult {
    pub success: bool,
    pub return_data: Bytes,
}

/// The slice of behaviour the exporter needs from a multicall driver. Behind a
/// trait so the scrape loop can be tested with a fake. (Go: `CallDriver`.)
#[allow(async_fn_in_trait)]
pub trait CallDriver: Send + Sync {
    async fn call(&self, block_number: u64, reqs: &[CallRequest]) -> Result<Vec<CallResult>>;
}

/// Returns (start, end) chunk ranges. `max == 0` disables chunking.
pub(crate) fn chunk_ranges(len: usize, max: usize) -> Vec<(usize, usize)> {
    if len == 0 {
        return Vec::new();
    }
    let chunk = if max == 0 || max > len { len } else { max };
    let mut ranges = Vec::new();
    let mut start = 0;
    while start < len {
        let end = (start + chunk).min(len);
        ranges.push((start, end));
        start += chunk;
    }
    ranges
}

/// Real driver over a live alloy provider.
pub struct Multicall3Driver<P: Provider + Clone> {
    provider: P,
    address: Address,
    max_calls_per_batch: usize,
}

impl<P: Provider + Clone> Multicall3Driver<P> {
    pub fn new(provider: P, address: Address, max_calls_per_batch: usize) -> Result<Self> {
        if address == Address::ZERO {
            bail!("multicall: multicall3 address is required");
        }
        Ok(Self {
            provider,
            address,
            max_calls_per_batch,
        })
    }

    async fn call_chunk(&self, block_number: u64, reqs: &[CallRequest]) -> Result<Vec<CallResult>> {
        let contract = IMulticall3::new(self.address, self.provider.clone());
        let calls: Vec<IMulticall3::Call3> = reqs
            .iter()
            .map(|r| IMulticall3::Call3 {
                target: r.target,
                allowFailure: true,
                callData: r.call_data.clone(),
            })
            .collect();
        let returned = contract
            .aggregate3(calls)
            .block(BlockId::number(block_number))
            .call()
            .await?;
        Ok(returned
            .into_iter()
            .map(|r| CallResult {
                success: r.success,
                return_data: r.returnData,
            })
            .collect())
    }
}

impl<P: Provider + Clone> CallDriver for Multicall3Driver<P> {
    async fn call(&self, block_number: u64, reqs: &[CallRequest]) -> Result<Vec<CallResult>> {
        if reqs.is_empty() {
            return Ok(Vec::new());
        }
        let mut results = Vec::with_capacity(reqs.len());
        for (start, end) in chunk_ranges(reqs.len(), self.max_calls_per_batch) {
            let chunk = self.call_chunk(block_number, &reqs[start..end]).await?;
            if chunk.len() != end - start {
                bail!(
                    "multicall chunk [{start}:{end}]: expected {} results, got {}",
                    end - start,
                    chunk.len()
                );
            }
            results.extend(chunk);
        }
        Ok(results)
    }
}

#[cfg(test)]
mod chunk_tests {
    use super::chunk_ranges;

    #[test]
    fn chunk_boundaries() {
        assert_eq!(chunk_ranges(0, 500), Vec::<(usize, usize)>::new());
        assert_eq!(chunk_ranges(3, 500), vec![(0, 3)]);
        assert_eq!(chunk_ranges(5, 2), vec![(0, 2), (2, 4), (4, 5)]);
        assert_eq!(chunk_ranges(4, 0), vec![(0, 4)]);
    }
}

#[cfg(test)]
mod driver_tests {
    use super::*;
    use alloy::primitives::U256;
    use alloy::providers::{ProviderBuilder, mock::Asserter};
    use alloy::sol_types::SolValue;

    fn mock_provider(asserter: Asserter) -> impl Provider + Clone {
        ProviderBuilder::new()
            .connect_mocked_client(asserter)
            .erased()
    }

    fn encode_results(items: &[(bool, Vec<u8>)]) -> Bytes {
        let results: Vec<IMulticall3::Result> = items
            .iter()
            .map(|(s, d)| IMulticall3::Result {
                success: *s,
                returnData: d.clone().into(),
            })
            .collect();
        (results,).abi_encode_params().into()
    }

    #[tokio::test]
    async fn returns_results_in_order() {
        let asserter = Asserter::new();
        let word = U256::from(7u64).abi_encode();
        asserter.push_success(&encode_results(&[(true, word.clone()), (false, vec![])]));
        let provider = mock_provider(asserter);
        let driver =
            Multicall3Driver::new(provider, CANONICAL_MULTICALL3.parse().expect("addr"), 500)
                .expect("driver");

        let reqs = vec![
            CallRequest {
                target: Address::repeat_byte(1),
                call_data: Bytes::new(),
            },
            CallRequest {
                target: Address::repeat_byte(2),
                call_data: Bytes::new(),
            },
        ];
        let out = driver.call(123, &reqs).await.expect("call");
        assert_eq!(out.len(), 2);
        assert!(out[0].success && !out[1].success);
        assert_eq!(out[0].return_data.as_ref(), word.as_slice());
    }
}
