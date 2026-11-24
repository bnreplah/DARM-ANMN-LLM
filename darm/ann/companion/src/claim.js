// Binds a DARM node id to an EVM wallet (spec section 12). The node signs a
// message that names the wallet, the registry contract and the EVM chain, so
// the claim cannot be replayed on another chain or contract or for another
// wallet. The wallet then publishes it with DarmRegistry.claimNode; anyone
// verifies the signature off-chain with `verifyClaim`.

import { fromHex } from './bytes.js';
import { Canonical } from './canonical.js';
import { sign, verify } from './identity.js';

const address = (hex) => {
  if (!/^0x[0-9a-fA-F]{40}$/.test(hex)) throw new RangeError('address must be 20 bytes of hex');
  return fromHex(hex.slice(2), 20);
};

export function claimMessage({ darmChain, node, evmChainId, registry, wallet }) {
  return new Canonical('evm-node-claim')
    .fixed32(darmChain)
    .fixed32(node)
    .u64(evmChainId)
    .bytes(address(registry))
    .bytes(address(wallet))
    .finish();
}

export async function signClaim(identity, fields) {
  return sign(identity, claimMessage({ ...fields, node: identity.nodeId }));
}

export const verifyClaim = (fields, signature) => verify(fields.node, claimMessage(fields), signature);
