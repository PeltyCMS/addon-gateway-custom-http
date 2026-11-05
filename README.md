# Custom HTTP gateway

Accept your HTTP endpoint payments in PeltyCMS checkout

Official payment gateway for [PeltyCMS](https://github.com/MythicalLTD/PeltyCMS).

## Why this addon

Hosts who want customers to pay invoices without leaving Pelty checkout.

## Features

- Take invoice and checkout payments through **your HTTP endpoint**
- Signed webhook handling so paid invoices mark paid automatically
- Fits Pelty tax, credits, refunds and multi-currency
- Runs as sandboxed WASM — no custom PHP on your panel
- Keys live in Pelty secrets, not in theme code

## How it works

1. Install **Custom HTTP gateway** from the [Mythical marketplace](https://market.mythicalsystems.org/marketplace/product/gateway-custom-http) inside Pelty Admin → Addons.
2. Open Pelty Admin → Payments, add your your HTTP endpoint keys, and enable the gateway on the products you sell.
3. Customers hit checkout / login / order flow as usual — your HTTP endpoint runs in the background.

## Requirements

- PeltyCMS with WASM addons enabled
- Working your HTTP endpoint credentials
- Outbound HTTPS from your panel

## Get it

**[Custom HTTP gateway on Mythical Marketplace](https://market.mythicalsystems.org/marketplace/product/gateway-custom-http)**

## Support

Bugs and questions for this addon → Issues on this repo.  
PeltyCMS core → https://github.com/MythicalLTD/PeltyCMS
