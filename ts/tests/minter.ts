import { spawn } from "child_process";
import { TestUtils } from "@chugach-foundation/cypher-client";
import { clientProvider } from "../src/utils";

const createTestUser = () =>
    TestUtils.createTestUser(
        clientProvider.provider,
        clientProvider.groupAddr,
        clientProvider.cluster
    );

async function minterLiquidation() {
    const initPrice = 20;
    const cAssetMint = (await clientProvider.cAssets())[0];

    const [minter1, minter2, trader1, trader2] = await Promise.all([
        createTestUser(),
        createTestUser(),
        createTestUser(),
        createTestUser(),
    ]);

    const minter1DebtShares = 1000;
    const minter1Collateral = minter1DebtShares * initPrice * 10;
    await minter1.faucetUSDC(minter1Collateral);
    await minter1.depositCollateral(cAssetMint, minter1Collateral);
    await minter1.mintCAssets(cAssetMint, minter1DebtShares, initPrice);

    const minter2DebtShares = 100;
    const minter2Collateral = minter2DebtShares * initPrice * 4;
    await minter2.faucetUSDC(minter2Collateral);
    await minter2.depositCollateral(cAssetMint, minter2Collateral);
    await minter2.mintCAssets(cAssetMint, minter2DebtShares, initPrice);

    const trader1Balance =
        (minter1DebtShares + minter2DebtShares) * initPrice * 1.05;
    const trader2Balance = 10000000;
    await trader1.faucetUSDC(trader1Balance);
    await trader1.depositUSDCToMarginAccount(trader1Balance);
    await trader2.faucetUSDC(trader2Balance);
    await trader2.depositUSDCToMarginAccount(trader2Balance);
    await trader1.placeOrderAndSettle(cAssetMint, {
        size: minter1DebtShares + minter2DebtShares,
        side: "buy",
        price: initPrice * 1.05,
        orderType: "limit",
        selfTradeBehavior: "decrementTake",
    });

    const raisedPrice = initPrice * 3;
    await trader1.placeOrder(cAssetMint, {
        size: minter1DebtShares + minter2DebtShares,
        side: "sell",
        price: raisedPrice,
        orderType: "postOnly",
        selfTradeBehavior: "decrementTake",
    });

    await trader2.placeOrderAndSettle(cAssetMint, {
        size: 1,
        side: "buy",
        price: raisedPrice,
        orderType: "limit",
        selfTradeBehavior: "decrementTake",
    });
}

async function runTest() {
    clientProvider.runCranker();
    const liqor = await clientProvider.loadLiqor();
    await liqor.liqor.faucetUSDC(10000000);
    await liqor.liqor.depositUSDCToMarginAccount(10000000);
    liqor.liqor.client.program.addEventListener(
        "LiquidateMintingPositionLog",
        (event) => {
            console.log("---------------");
            console.log(event.cypherGroup.toString());
            console.log(event.liqeeUser.toString());
            console.log(event.liqorUser.toString());
            console.log(event.cAssetMint.toString());
            console.log(event.preCollateral.toNumber());
            console.log(event.preDebtShares.toNumber());
            console.log(event.repayAmount.toNumber());
            console.log(event.collToLiqor.toNumber());
            console.log(event.collToProtocol.toNumber());
        }
    );

    const liquidator = spawn("yarn", ["liquidator"], {
        env: {
            ANCHOR_WALLET: process.env.ANCHOR_WALLET,
            PATH: process.env.PATH,
        },
    });

    liquidator.stdout.on("data", (data) => {
        console.log(`Liquidator stdout: ${data}`);
    });

    liquidator.stderr.on("data", (data) => {
        console.error(`Liquidator stderr: ${data}`);
    });

    liquidator.on("close", (code) => {
        console.log(`Liquidator exited with code ${code}`);
    });
    // console.log("+++++++++++++");
    await minterLiquidation();
}

runTest();
