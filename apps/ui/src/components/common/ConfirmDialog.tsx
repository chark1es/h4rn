import { useTranslation } from "react-i18next";

import {
  AlertDialog,
  AlertDialogClose,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
} from "@/components/ui/alert-dialog";
import { buttonVariants } from "@/components/ui/button";

/** A small destructive-action confirmation shared by Changes and Forges. */
export function ConfirmDialog({
  open,
  title,
  description,
  confirmLabel,
  onOpenChange,
  onConfirm,
}: {
  open: boolean;
  title: string;
  description: string;
  confirmLabel: string;
  onOpenChange: (open: boolean) => void;
  onConfirm: () => void;
}) {
  const { t } = useTranslation("common");
  return (
    <AlertDialog
      open={open}
      onOpenChange={(next, details) => {
        onOpenChange(next);
        if (!next && details.returnValue === "confirm") onConfirm();
      }}
    >
      <AlertDialogContent>
        <AlertDialogHeader>
          <AlertDialogTitle>{title}</AlertDialogTitle>
          <AlertDialogDescription>{description}</AlertDialogDescription>
        </AlertDialogHeader>
        <AlertDialogFooter>
          <AlertDialogClose className={buttonVariants({ variant: "secondary" })}>
            {t("cancel")}
          </AlertDialogClose>
          <AlertDialogClose value="confirm" className={buttonVariants({ variant: "destructive" })}>
            {confirmLabel}
          </AlertDialogClose>
        </AlertDialogFooter>
      </AlertDialogContent>
    </AlertDialog>
  );
}
