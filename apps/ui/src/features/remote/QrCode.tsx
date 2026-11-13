import qrcode from "qrcode-generator";

/**
 * Renders a real QR code.
 *
 * The pairing payload is a `h4rn://pair?...` URL minted by h4rn-connect, so it
 * must be encoded, not drawn from a seed: a stand-in pattern does not scan.
 * `qrcode-generator` is dependency-free and runs in the webview.
 */
export function QrCode({
  value,
  label,
  size = 188,
}: {
  value: string;
  label: string;
  size?: number;
}) {
  let modules: boolean[][] = [];
  try {
    const code = qrcode(0, "M");
    code.addData(value);
    code.make();
    const count = code.getModuleCount();
    modules = Array.from({ length: count }, (_, row) =>
      Array.from({ length: count }, (_, column) => code.isDark(row, column)),
    );
  } catch {
    // A payload too large for a QR falls back to the empty frame below; the
    // link is still shown as text beside it.
    modules = [];
  }
  const count = modules.length;
  const quiet = 2;
  const view = count + quiet * 2;
  const cells: React.ReactNode[] = [];
  for (let row = 0; row < count; row += 1) {
    for (let column = 0; column < count; column += 1) {
      if (!modules[row]?.[column]) continue;
      cells.push(
        <rect
          key={`${row}-${column}`}
          x={column + quiet}
          y={row + quiet}
          width={1}
          height={1}
          fill="#111"
        />,
      );
    }
  }
  return (
    <svg
      viewBox={`0 0 ${view || 1} ${view || 1}`}
      width={size}
      height={size}
      shapeRendering="crispEdges"
      role="img"
      aria-label={label}
    >
      <rect width={view || 1} height={view || 1} fill="#fff" />
      {cells}
    </svg>
  );
}
