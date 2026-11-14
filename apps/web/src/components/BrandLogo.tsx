import logoUrl from "../../../../assets/branding/logo.svg?no-inline";

/** 品牌 Logo 画布为横向 1260×900：按高度等比显示，避免方形槽位把云朵拉伸。 */
export function BrandLogo({ height }: { height: number }) {
  return <img src={logoUrl} alt="" style={{ height, width: "auto", aspectRatio: "1260 / 900", objectFit: "contain" }} />;
}
