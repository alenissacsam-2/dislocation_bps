# Decoder fixtures

Real mainnet account bytes, committed so decoder tests never touch the network.

Capture a fixture with:

```bash
solana account <POOL_ADDRESS> --output json --output-file <name>.json --url mainnet-beta
```

Record the slot it was captured at in the filename: layouts change across program
upgrades, and a fixture without a slot is unfalsifiable.
