import { stubPhoneLayout, stubPopupApis } from "@/test/popups";

if (typeof window !== "undefined") {
  if (typeof window.matchMedia !== "function") stubPhoneLayout(false);
  stubPopupApis();
}
