import { sleep } from "@project-serum/common";
import { runLiquidator } from "./liquidator";
import { clientProvider } from "./utils";

const run = async () => {
    const interval = parseInt(process.env.LIQUIDATOR_INTERVAL!) || 60000;
    const liqor = await clientProvider.loadLiqor();
    liqor.liqor.client.program.addEventListener(
        "LiquidateMintingPositionLog",
        (event) => {
            console.log("====== LiquidateMintingPositionLog ========");
            console.log("cypher_group ", event.cypherGroup.toString());
            console.log("liqee_user ", event.liqeeUser.toString());
            console.log("liqor_user ", event.liqorUser.toString());
            console.log("c_asset_mint ", event.cAssetMint.toString());
            console.log("pre_collateral ", event.preCollateral.toNumber());
            console.log("pre_debt_shares ", event.preDebtShares.toNumber());
            console.log("repay_amount ", event.repayAmount.toNumber());
            console.log("coll_to_liqor ", event.collToLiqor.toNumber());
            console.log("coll_to_protocol ", event.collToProtocol.toNumber());
            console.log("============================================");
        }
    );
    while (true) {
        const userAddrs = await clientProvider.getAllUserAddresses();
        console.log("Start processing ", userAddrs.length, " users.");
        for (const userAddr of userAddrs) {
            await runLiquidator(userAddr);
        }
        await sleep(interval);
    }
};

// run();
